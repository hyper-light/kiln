//! How many agent sessions a machine has at work at once (docs/agents.md, "How many agents a
//! machine runs"). Only those at work count ([`crate::harness::session::Info::at_work`]: starting,
//! on a turn, waiting on a person in the middle of one, or with subagents still at work), not those
//! finished, ready, paused or ended. A session new to the machine while that many are at work (or
//! waiting for room) is refused, saying so. One it has goes to work again only with room (its
//! machine's [`crate::harness::host::Host::room_for`]): what it's sent waits in its queue till
//! then, the longest waiting first. Those at work go on whatever the limit says, and every session
//! comes back after a restart. Its limit is, the first of these there is:
//!
//! 1. what a person set for it from the server (Runners, the runner's own section; `PUT
//!    /api/runners/:name/agent-limit`): kept in the server's `kv` table (`agent_limit/<runner>`),
//!    sent to its runner as that says it takes it (`CAP_AGENT_LIMIT`) and whenever it changes
//!    (`ServerMsg::AgentLimit`), and committed by the runner in its host Amphora machine namespace;
//! 2. its config's `[agent] max_agent_sessions` (a `kiln agent`'s config file, read again within
//!    seconds of a change; the server's own `kiln.toml` for the server's own agents, read as it
//!    starts);
//! 3. kiln's default, [`DEFAULT`].
//!
//! A runner says what it holds and why (`AgentMsg::AgentLimit`) as it's welcomed and whenever that
//! changes. The server's own runner (`[local_agent]`, in its process) runs no agents: the server
//! runs its own, and what's set for its runner is theirs, held by the server itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use http::StatusCode;
use kiln_seams::records::{Cond, Consistency, Records, Refusal, Tx, Write};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::instances::PeerId;
use crate::proto::{CAP_AGENT_LIMIT, ServerMsg};

/// The most agent sessions a machine has at work at once when nothing says otherwise.
pub const DEFAULT: usize = 64;
/// The most a limit can be: past it, a slip of the keyboard, not a machine's room.
pub const MAX: usize = 4096;

/// What a person set from the server for a machine (`ServerMsg::AgentLimit`, `PUT
/// /api/runners/:name/agent-limit`): none, and its config's (or kiln's default) holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asked {
    #[serde(default)]
    pub max_sessions: Option<usize>,
}

/// Where a machine's limit comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A person set it from the server.
    Server,
    /// Its config's `[agent] max_agent_sessions`.
    Config,
    /// Kiln's own, [`DEFAULT`].
    Default,
}

/// The limit a machine holds its agent sessions to, and where it comes from
/// (`AgentMsg::AgentLimit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub max_sessions: usize,
    pub from: Source,
    /// Its config's, when it says one: what it holds once nothing is set from the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<usize>,
}

/// `n` as a limit, or why it can't be one.
pub fn valid(n: usize) -> Result<usize, String> {
    if (1..=MAX).contains(&n) { Ok(n) } else { Err(format!("a machine has from 1 to {MAX} agent sessions at work at once, not {n}")) }
}

/// What a machine holds: what's set from the server, else its config's, else kiln's default. A
/// number past the bounds ([`valid`]: a config's 0, say) is passed over.
pub fn held(asked: Asked, config: Option<usize>) -> Held {
    let config = config.filter(|n| valid(*n).is_ok());
    match (asked.max_sessions.filter(|n| valid(*n).is_ok()), config) {
        (Some(n), _) => Held { max_sessions: n, from: Source::Server, config },
        (None, Some(n)) => Held { max_sessions: n, from: Source::Config, config },
        (None, None) => Held { max_sessions: DEFAULT, from: Source::Default, config: None },
    }
}

// ------------------------------------------------------------------------------ on the runner

/// U17: retired read-only input; accepted policy now shares the host machine namespace.
fn kept_path(state_dir: &Path) -> PathBuf {
    state_dir.join("agent-limit.json")
}

const POLICY_FAMILY: &str = "agent-limit";
const POLICY_KEY: &[u8] = b"accepted-v1";
const POLICY_BYTES: u64 = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Accepted {
    version: u32,
    /// None records that the legacy file was absent; otherwise its exact SHA256.
    legacy_sha256: Option<String>,
    asked: Asked,
}

fn checked(asked: Asked) -> anyhow::Result<Asked> {
    if let Some(n) = asked.max_sessions { valid(n).map_err(anyhow::Error::msg)?; }
    Ok(asked)
}

impl Accepted {
    fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(bytes.len() <= POLICY_BYTES as usize, "U17 agent limit record exceeds its bound");
        let value: Self = serde_json::from_slice(bytes).context("U17 unreadable accepted agent limit")?;
        anyhow::ensure!(value.version == 1, "U17 unsupported agent limit record version");
        anyhow::ensure!(value.legacy_sha256.as_ref().is_none_or(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))), "U17 invalid agent limit import receipt");
        checked(value.asked)?;
        Ok(value)
    }
}

/// Bounded, strict and read-only: malformed policy must never become an empty policy.
fn legacy_policy(path: &Path) -> anyhow::Result<(Asked, Option<String>)> {
    use std::io::Read as _;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Asked::default(), None)),
        Err(e) => return Err(e).context("U17 cannot inspect legacy agent limit"),
    };
    anyhow::ensure!(metadata.is_file() && metadata.len() <= POLICY_BYTES, "U17 legacy agent limit must be a regular file of at most {POLICY_BYTES} bytes");
    let file = std::fs::File::open(path).context("U17 cannot read legacy agent limit")?;
    anyhow::ensure!(file.metadata()?.is_file(), "U17 legacy agent limit is not a regular file");
    let mut bytes = Vec::new();
    file.take(POLICY_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= POLICY_BYTES as usize, "U17 legacy agent limit exceeds {POLICY_BYTES} bytes");
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Legacy {
        #[serde(default)]
        max_sessions: Option<usize>,
    }
    let value: Legacy = serde_json::from_slice(&bytes).context("U17 unreadable legacy agent limit")?;
    Ok((checked(Asked { max_sessions: value.max_sessions })?, Some(crate::util::sha256_hex(&bytes))))
}

/// U17: accepted runner projection, separate from the server's authoritative desired policy.
/// The existing Records adapter moves every metadata call off the runtime shard.
pub struct RunnerPolicy {
    records: Arc<dyn Records>,
    accepted: Asked,
}

impl RunnerPolicy {
    /// Complete policy load/import before restoring sessions. No unreadable-state fallback.
    pub async fn from_database(database: &amphora::database::Database, namespace: &str, state_dir: &Path) -> anyhow::Result<Self> {
        Self::load(Arc::new(crate::records::from_database(database, namespace)?), kept_path(state_dir)).await
    }

    async fn load(records: Arc<dyn Records>, path: PathBuf) -> anyhow::Result<Self> {
        for _ in 0..8 {
            if let Some(row) = records.get(POLICY_FAMILY, POLICY_KEY, Consistency::Linearizable).await? {
                return Ok(Self { accepted: Accepted::decode(&row.value)?.asked, records });
            }
            let source = path.clone();
            let (asked, legacy_sha256) = crate::rt::blocking(move || legacy_policy(&source)).await??;
            let source = path.clone();
            let checked_source = crate::rt::blocking(move || legacy_policy(&source)).await??;
            anyhow::ensure!(checked_source == (asked, legacy_sha256.clone()), "U17 legacy agent limit changed during import");
            let value = serde_json::to_vec(&Accepted { version: 1, legacy_sha256, asked })?;
            let tx = Tx {
                conds: vec![Cond::Absent { family: POLICY_FAMILY.into(), key: POLICY_KEY.into() }],
                writes: vec![Write::Put { family: POLICY_FAMILY.into(), key: POLICY_KEY.into(), value }],
            };
            // The import receipt and accepted value publish together; no partial import exists.
            match records.commit(tx).await {
                Ok(_) => return Ok(Self { records, accepted: asked }),
                Err(Refusal::Conflict { .. }) => continue,
                Err(e) => return Err(e).context("U17 agent limit import was not acknowledged"),
            }
        }
        anyhow::bail!("U17 agent limit import conflicted repeatedly")
    }

    pub fn asked(&self) -> Asked { self.accepted }

    /// Commit before publishing or acknowledging. Failure preserves this process's last held
    /// policy, including an ambiguous commit outcome; a fresh load reconciles durable state.
    pub async fn accept(&mut self, asked: Asked) -> anyhow::Result<Asked> {
        let asked = checked(asked)?;
        for _ in 0..8 {
            let row = self.records.get(POLICY_FAMILY, POLICY_KEY, Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("U17 accepted agent limit disappeared"))?;
            let mut value = Accepted::decode(&row.value)?;
            if value.asked != asked {
                value.asked = asked;
                let tx = Tx {
                    conds: vec![Cond::At { family: POLICY_FAMILY.into(), key: POLICY_KEY.into(), rev: row.rev }],
                    writes: vec![Write::Put { family: POLICY_FAMILY.into(), key: POLICY_KEY.into(), value: serde_json::to_vec(&value)? }],
                };
                match self.records.commit(tx).await {
                    Ok(_) => {},
                    Err(Refusal::Conflict { .. }) => continue,
                    Err(e) => return Err(e).context("U17 agent limit update was not acknowledged"),
                }
            }
            self.accepted = asked;
            return Ok(asked);
        }
        anyhow::bail!("U17 agent limit update conflicted repeatedly")
    }
}

/// The config file at `path`'s `[agent] max_agent_sessions` (`Some(None)`: it says none, or there's
/// no file), or nothing while it can't be read as kiln's config (a change half written): the last
/// one read holds then. Blocking.
pub fn configured(path: &Path) -> Option<Option<usize>> {
    crate::config::Config::load(path).ok().map(|c| c.agent.max_agent_sessions)
}

// ------------------------------------------------------------------------------ on the server

const KEY_PREFIX: &str = "agent_limit/";
/// A runner process that has said nothing past `Build`'s 32 bits this long after connecting never
/// will: it is from before the channel, so from before agent limits.
const CAPS_GRACE: i64 = 15;

pub type Answer = Result<Response, (StatusCode, String)>;

fn fail(code: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (code, msg.into())
}

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    asked: Asked,
    /// What its runner last said it holds.
    held: Option<Held>,
}

/// What's set from the server for each machine, and what each said it holds.
#[derive(Default)]
pub struct AgentLimits {
    inner: Mutex<HashMap<String, Entry>>,
}

impl AgentLimits {
    /// What the UI set, read as the server starts.
    pub async fn load(&self, store: &crate::store::Store) -> anyhow::Result<()> {
        let rows = store.kv_prefix(KEY_PREFIX.to_owned()).await?;
        let mut g = self.inner.lock();
        for (k, v) in rows {
            let Some(machine) = k.strip_prefix(KEY_PREFIX) else { continue };
            match serde_json::from_str::<Asked>(&v) {
                Ok(asked) => g.entry(machine.to_owned()).or_default().asked = asked,
                Err(e) => tracing::warn!(machine, error = %e, "agent limit: what was set is unreadable; that machine's config holds"),
            }
        }
        Ok(())
    }

    /// What's set from the server for `machine`.
    pub fn asked(&self, machine: &str) -> Asked {
        self.inner.lock().get(machine).map(|e| e.asked).unwrap_or_default()
    }

    fn held(&self, machine: &str) -> Option<Held> {
        self.inner.lock().get(machine).and_then(|e| e.held)
    }

    /// A machine forgotten (its runner was): what was set for it goes.
    pub async fn forget(&self, store: &crate::store::Store, machine: &str) {
        self.inner.lock().remove(machine);
        if let Err(e) = store.kv_delete(format!("{KEY_PREFIX}{machine}")).await {
            tracing::warn!(machine, error = %format!("{e:#}"), "agent limit: what was set wasn't deleted");
        }
    }
}

/// Whether `machine` is the server's own runner (`[local_agent]`, in its process): it runs no
/// agents, and the server's own agents are held to what's set for it.
fn own(hub: &Hub, machine: &str) -> bool {
    hub.cfg.local_agent.enabled && machine == crate::pki::sanitize_name(&hub.cfg.local_agent_name())
}

/// What the server's own agents hold: what's set for the server's own runner, else its config's
/// `[agent] max_agent_sessions`, else kiln's default.
fn local(hub: &Hub) -> Held {
    let asked = if hub.cfg.local_agent.enabled { hub.agent_limits.asked(&crate::pki::sanitize_name(&hub.cfg.local_agent_name())) } else { Asked::default() };
    held(asked, hub.cfg.agent.max_agent_sessions)
}

/// The server's own agents held to what they hold ([`local`]) from now on.
pub fn hold_local(hub: &Hub) {
    let h = local(hub);
    if hub.harness.local.set_max_sessions(h.max_sessions) {
        tracing::info!(max_sessions = h.max_sessions, from = ?h.from, "the server's own agents: at most this many at work at once");
    }
}

/// The runner processes of `machine` that take its agent limit (each runs sessions of its own).
fn takers(hub: &Hub, machine: &str) -> Vec<PeerId> {
    let inst = hub.instances.lock();
    inst.of(machine).into_iter().filter(|p| inst.get(*p).is_some_and(|i| i.wide & CAP_AGENT_LIMIT != 0)).collect()
}

/// Sends what's set for `machine` to runner processes `peers` of it (a state: the newest goes,
/// answered once taken). Whether it's on its way: its answer is logged when it comes.
async fn send(hub: &Hub, machine: &str, peers: Vec<PeerId>) -> bool {
    let Ok(json) = serde_json::to_string(&hub.agent_limits.asked(machine)) else { return false };
    let pending = hub.command_peers(machine, peers, &ServerMsg::AgentLimit { json }).await;
    let on_its_way = pending.at_once().is_none_or(|a| matches!(a, crate::wire::commands::Answer::Sent));
    pending.log_when_answered(crate::hub::LOG_WITHIN);
    on_its_way
}

/// What's set for `machine`, held from now on: by the server's own agents at once, or sent to the
/// machine's runner processes that take it (those that don't, or a machine that's away, get it as
/// they say they take it). Whether something holds it or has it on its way.
pub async fn push(hub: &Hub, machine: &str) -> bool {
    if own(hub, machine) {
        hold_local(hub);
        return true;
    }
    let peers = takers(hub, machine);
    !peers.is_empty() && send(hub, machine, peers).await
}

/// A runner process said what it can do past `Build`'s 32 bits (`AgentMsg::Caps`): one that takes
/// the agent limit is sent what's set for its machine, none set too (what it kept from before goes).
pub async fn on_caps(hub: &Hub, machine: &str, peer: PeerId, caps: u64) {
    if caps & CAP_AGENT_LIMIT != 0 {
        send(hub, machine, vec![peer]).await;
    }
}

/// A runner said what it holds (`AgentMsg::AgentLimit`, read as `crate::wire::payload` reads what
/// a frame carries: one that can't be read is refused to the runner there): the process new work
/// goes to speaks for its machine.
pub fn on_held(hub: &Hub, peer: PeerId, machine: &str, held: Held) {
    if hub.instances.lock().newest(machine) != Some(peer) {
        return;
    }
    let changed = {
        let mut g = hub.agent_limits.inner.lock();
        let e = g.entry(machine.to_owned()).or_default();
        let changed = e.held != Some(held);
        e.held = Some(held);
        changed
    };
    if changed {
        hub.emit(&crate::wire::events::ServerEvent::AgentLimit { runner: machine.to_owned() });
    }
}

/// Whether the process `machine`'s new work goes to takes a limit from here: unknown until it has
/// said what it can do past `Build`'s 32 bits (its `Caps`, right after its `Build`), or is known to
/// be too old to (one from before the channel never says).
fn takes(hub: &Hub, machine: &str) -> Option<bool> {
    let inst = hub.instances.lock();
    let i = inst.newest(machine).and_then(|p| inst.get(p))?;
    if i.wide != 0 {
        return Some(i.wide & CAP_AGENT_LIMIT != 0);
    }
    (i.id.is_none() || crate::util::now_unix() - i.connected_at > CAPS_GRACE).then_some(false)
}

/// A runner the server knows (enrolled, or connected now).
async fn known(hub: &Hub, name: &str) -> bool {
    hub.agents.lock().contains_key(name) || hub.store.agents().await.is_ok_and(|rows| rows.iter().any(|a| a.name == name))
}

/// `GET /api/runners/:name/agent-limit` (and what `PUT` answers): what's set here, what the machine
/// holds and why, how many of its agent sessions are at work now (what its limit counts) and how
/// many it has in all, finished, paused and ended ones too.
pub async fn view(hub: &Hub, name: &str) -> Value {
    let own = own(hub, name);
    let (connected, runs_agents) = hub.agents.lock().get(name).filter(|a| a.connected).map_or((false, None), |a| (true, Some(!a.ephemeral && a.terminal)));
    let (supported, held) = if own {
        (Some(true), Some(local(hub)))
    } else {
        let supported = if connected { takes(hub, name) } else { None };
        // A runner that came back too old to take one holds kiln's default, whatever it said before.
        (supported, hub.agent_limits.held(name).filter(|_| supported != Some(false)))
    };
    let target = if own { "local".to_owned() } else { format!("agent:{name}") };
    let sessions: Vec<_> = hub.harness.list().into_iter().filter(|i| i.target == target).collect();
    // At work now, and waiting for room to go to work, as its sessions stand here; not known while
    // it's away.
    let here = own || connected;
    let working = here.then(|| sessions.iter().filter(|i| i.at_work()).count());
    let waiting = here.then(|| sessions.iter().filter(|i| i.room_wait.is_some()).count());
    json!({
        "runner": name,
        "own": own,
        "connected": connected,
        "runs_agents": if own { Some(true) } else { runs_agents },
        "supported": supported,
        "max_sessions": hub.agent_limits.asked(name).max_sessions,
        "held": held,
        "working": working,
        "waiting": waiting,
        "sessions": sessions.len(),
        "default": DEFAULT,
        "max": MAX,
    })
}

pub async fn get(hub: &Hub, name: &str) -> Answer {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    Ok(srv::json(StatusCode::OK, &view(hub, name).await))
}

/// What a `PUT` asks for: `{"max_sessions": n}`, or `{"max_sessions": null}` for nothing set here.
fn asked_of(body: &Value) -> Result<Asked, String> {
    const SHAPE: &str = "expected {\"max_sessions\": n}, or {\"max_sessions\": null} for its config's";
    let o = body.as_object().ok_or(SHAPE)?;
    if let Some(k) = o.keys().find(|k| *k != "max_sessions") {
        return Err(format!("{k:?} isn't one: {SHAPE}"));
    }
    match o.get("max_sessions").ok_or(SHAPE)? {
        Value::Null => Ok(Asked::default()),
        v => {
            let n = v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| format!("max_sessions is a whole number of agent sessions, not {v}"))?;
            Ok(Asked { max_sessions: Some(valid(n)?) })
        }
    }
}

/// `PUT /api/runners/:name/agent-limit`: `{"max_sessions": n}` sets it, `{"max_sessions": null}`
/// takes what was set away (its config's holds again); checked, stored, and sent to the machine
/// (held as it next connects if it's away).
pub async fn put(hub: &Hub, name: &str, req: Request) -> Answer {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    let body: Value = srv::body_json(req.into_body(), 4096).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let asked = asked_of(&body).map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let key = format!("{KEY_PREFIX}{name}");
    let stored = match asked.max_sessions {
        Some(_) => match serde_json::to_string(&asked) {
            Ok(json) => hub.store.kv_set(key, json).await,
            Err(e) => Err(e.into()),
        },
        None => hub.store.kv_delete(key).await,
    };
    stored.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.agent_limits.inner.lock().entry(name.to_owned()).or_default().asked = asked;
    let said = asked.max_sessions.map_or_else(|| "nothing set here: its config's".to_owned(), |n| format!("at most {n} agent sessions at work at once"));
    hub.store.audit("admin".into(), "agent_limit.update".into(), format!("{name}: {said}")).await;
    push(hub, name).await;
    hub.emit(&crate::wire::events::ServerEvent::AgentLimit { runner: name.to_owned() });
    Ok(srv::json(StatusCode::OK, &view(hub, name).await))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::future::BoxFuture;
    use kiln_seams::records::{Changes, Page, Rev, Row, Scan, SimRecords};

    use super::*;

    /// What's set from the server wins over the config's, which wins over kiln's default; a number
    /// out of bounds is passed over.
    #[test]
    fn what_is_set_here_wins_over_the_config_which_wins_over_the_default() {
        let set = |n| Asked { max_sessions: Some(n) };
        let h = |max_sessions, from, config| Held { max_sessions, from, config };
        assert_eq!(held(set(128), Some(100)), h(128, Source::Server, Some(100)));
        assert_eq!(held(set(8), None), h(8, Source::Server, None));
        assert_eq!(held(Asked::default(), Some(100)), h(100, Source::Config, Some(100)));
        assert_eq!(held(Asked::default(), None), h(DEFAULT, Source::Default, None));
        assert_eq!(held(Asked::default(), Some(0)), h(DEFAULT, Source::Default, None), "a config's 0 isn't a limit");
        assert_eq!(held(set(0), Some(100)).from, Source::Config, "nor is 0 set here");
        assert_eq!(held(set(MAX + 1), None).max_sessions, DEFAULT);
        assert_eq!(held(set(MAX), None).max_sessions, MAX);
        assert!(valid(0).is_err() && valid(MAX + 1).is_err() && valid(1).is_ok());
    }

    /// What a runner says goes as JSON both ways; a source reads as the UI names it.
    #[test]
    fn what_a_runner_holds_reads_as_the_ui_names_it() -> anyhow::Result<()> {
        let h = Held { max_sessions: 128, from: Source::Server, config: Some(100) };
        assert_eq!(serde_json::to_value(h)?, json!({"max_sessions": 128, "from": "server", "config": 100}));
        assert_eq!(serde_json::to_value(held(Asked::default(), None))?, json!({"max_sessions": DEFAULT, "from": "default"}));
        assert_eq!(serde_json::from_str::<Held>(r#"{"max_sessions":5,"from":"config"}"#)?, Held { max_sessions: 5, from: Source::Config, config: None });
        assert_eq!(serde_json::from_str::<Asked>("{}")?, Asked::default());
        Ok(())
    }

    /// A `PUT` sets a number from 1 to MAX, or `null`; anything else (a key misspelt, which would
    /// otherwise take what was set away) is refused, saying why.
    #[test]
    fn a_put_sets_a_number_or_null_and_nothing_else() {
        assert_eq!(asked_of(&json!({"max_sessions": 128})), Ok(Asked { max_sessions: Some(128) }));
        assert_eq!(asked_of(&json!({"max_sessions": null})), Ok(Asked::default()));
        for bad in [json!({}), json!({"max_session": 128}), json!({"max_sessions": 128, "max_agents": 4}), json!({"max_sessions": 0}), json!({"max_sessions": -3}), json!({"max_sessions": 2.5}), json!({"max_sessions": "lots"}), json!({"max_sessions": MAX + 1}), json!([128])] {
            assert!(asked_of(&bad).is_err(), "{bad} was taken");
        }
        assert!(asked_of(&json!({"max_session": 128})).is_err_and(|e| e.contains("\"max_session\" isn't one")));
    }

    /// U17: import is read-only, and clearing policy remains durable across real host reopens;
    /// the retired legacy value must never resurrect what the server has cleared.
    #[test]
    fn u17_agent_limit_a_machine_keeps_what_was_set_for_its_restarts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let legacy = br#"{"max_sessions":200}"#;
        std::fs::write(kept_path(dir.path()), legacy)?;
        let path = dir.path().join("kiln.db");
        let store = crate::store::Store::open(&path)?;
        let state_dir = dir.path().to_owned();
        crate::rt::block_on(async move {
            let mut policy = RunnerPolicy::from_database(&store.database(), "machine.test-limit", &state_dir).await?;
            assert_eq!(policy.asked(), Asked { max_sessions: Some(200) });
            assert_eq!(policy.accept(Asked { max_sessions: Some(2) }).await?, Asked { max_sessions: Some(2) });
            policy.accept(Asked::default()).await?;
            anyhow::Ok(())
        })?;
        assert_eq!(std::fs::read(kept_path(dir.path()))?, legacy.to_vec(), "legacy input was never rewritten or removed");
        let store = crate::store::Store::open(&path)?;
        let state_dir = dir.path().to_owned();
        crate::rt::block_on(async move {
            let policy = RunnerPolicy::from_database(&store.database(), "machine.test-limit", &state_dir).await?;
            assert_eq!(policy.asked(), Asked::default(), "clear is canonical even while old JSON still says 200");
            anyhow::Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn u17_agent_limit_bad_legacy_refuses_without_a_receipt() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let records = Arc::new(SimRecords::default());
        crate::rt::block_on(async move {
            for bad in ["not json".to_owned(), r#"{"max_sessions":0}"#.to_owned(), r#"{"max_sessions":4097}"#.to_owned(), r#"{"max_session":2}"#.to_owned(), r#"{"max_sessions":2,"max_sessions":3}"#.to_owned(), " ".repeat(POLICY_BYTES as usize + 1)] {
                std::fs::write(kept_path(dir.path()), bad)?;
                assert!(RunnerPolicy::load(records.clone(), kept_path(dir.path())).await.is_err());
                assert_eq!(records.head().await?, 0, "invalid input must not publish an import receipt");
            }
            anyhow::Ok(())
        })
    }

    /// A failed commit must leave the effective policy and the next process's loaded policy at
    /// the last accepted value; even initial import failure must prevent policy construction.
    #[test]
    fn u17_agent_limit_commit_refusal_keeps_last_held_policy() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let records = Arc::new(RefusingCommits { inner: SimRecords::default(), fail: AtomicBool::new(true) });
        crate::rt::block_on(async move {
            assert!(RunnerPolicy::load(records.clone(), kept_path(dir.path())).await.is_err());
            assert_eq!(records.head().await?, 0);
            records.fail.store(false, Ordering::SeqCst);
            let mut policy = RunnerPolicy::load(records.clone(), kept_path(dir.path())).await?;
            let accepted = Asked { max_sessions: Some(2) };
            policy.accept(accepted).await?;
            records.fail.store(true, Ordering::SeqCst);
            assert!(policy.accept(Asked { max_sessions: Some(200) }).await.is_err());
            assert_eq!(policy.asked(), accepted);
            assert_eq!(held(policy.asked(), None).max_sessions, 2);
            let restarted = RunnerPolicy::load(records.clone(), kept_path(dir.path())).await?;
            assert_eq!(restarted.asked(), accepted);
            anyhow::Ok(())
        })
    }

    struct RefusingCommits { inner: SimRecords, fail: AtomicBool }
    impl Records for RefusingCommits {
        fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>> { self.inner.get(family, key, at) }
        fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Refusal>> { self.inner.scan(family, scan) }
        fn commit(&self, tx: Tx) -> BoxFuture<'_, Result<Rev, Refusal>> {
            if self.fail.load(Ordering::SeqCst) { Box::pin(async { Err(Refusal::Unavailable("injected policy commit failure".into())) }) } else { self.inner.commit(tx) }
        }
        fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Refusal>> { self.inner.changes(family, after, limit) }
        fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Refusal>> { self.inner.wait(family, after) }
        fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>> { self.inner.head() }
    }

    /// A runner's config is read again as it changes; one that can't be read keeps the last.
    #[test]
    fn a_config_says_its_limit_or_nothing_while_it_cant_be_read() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("kiln.toml");
        assert_eq!(configured(&path), Some(None), "no file: kiln's defaults");
        std::fs::write(&path, "[agent]\nmax_agent_sessions = 150\n\n[agent.facts]\nlocation = \"rack-2\"\n")?;
        assert_eq!(configured(&path), Some(Some(150)));
        std::fs::write(&path, "[agent]\nmax_agent_sessions = 15")?;
        assert_eq!(configured(&path), Some(Some(15)));
        std::fs::write(&path, "[agent]\nmax_agent_sessions = \"lots\"\n")?;
        assert_eq!(configured(&path), None, "a config kiln can't read says nothing");
        Ok(())
    }
}
