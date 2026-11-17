//! Agent definitions people keep in kiln (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P5.1): an agent
//! as data. One of kiln's harnesses (`crate::harness::kinds::Kind`, its fixed ten) and its model, the
//! agent's instructions, its tools and MCP servers, its skills, its limits (time, cost, tokens) and
//! the Agentfile its microVM is made from (one of its owner's, or kiln's for its harness), kept as
//! Agentfiles are (`crate::agentfiles`): each a person's own (`owner`: the server's `owner`, or
//! `user:<id>`), named once among theirs, every change of its settings a version, a restore too, a
//! rename alone none. A person reaches only their own; the server's owner, and whoever has
//! `agentdefs.manage` (an admin), reach everyone's, and another's is as none to the rest. Its settings
//! are checked before they're kept ([`check`]). Running one starts an agent session with its
//! settings, in its Agentfile's microVM, through the one start path (`agents.start`). Beside a
//! person's own, read only, are kiln's ([`builtin`]), which `copy` makes theirs. The same
//! operations serve the console (`/api/agentdefs`), agents (kiln's MCP server: `agentdefs_*`) and code
//! mode (`kiln.agentdefs.*`): [`ops`]. kiln.db keeps them: a definition is small, its settings JSON.
//! Its person's experiments split its new sessions between deployments ([`experiments`]).

pub mod builtin;
pub mod experiments;
pub mod limits;
pub mod ops;
pub mod sessions;
pub mod sync;
pub mod uses;
pub mod volumes;

use rusqlite::{OptionalExtension as _, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::authz::{Caller, Scope};
use crate::harness::engine::Engine;
use crate::harness::kinds::Kind;
use crate::hub::Hub;

/// What reaches everyone's agent definitions (an admin's, as the role has every capability).
pub const MANAGE: &str = "agentdefs.manage";

/// A MiB, in bytes: a microVM's memory is a whole number of them, as a start sizes it.
pub const MIB: u64 = 1 << 20;

/// An MCP server an agent has: by its URL, or by the command that starts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mcp {
    /// Its name, once among the agent's.
    pub name: String,
    /// Where it answers (Streamable HTTP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The command that starts it (stdio), and its arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
}

/// The most an agent's run may take: each left out, no limit of the definition's own. Each is held to the sessions
/// it starts ([`limits`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Its time, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u64>,
    /// Its cost, in US cents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cents: Option<u64>,
    /// Its tokens, in and out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

/// What an agent is: its settings, every version of which is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// One of kiln's harnesses (`claude`, `codex`, `devin`, `gemini`, `opencode`, `goose`, `amp`,
    /// `antigravity`, `cursor`, `pi`).
    pub harness: String,
    /// Its model; none: its harness's default.
    #[serde(default)]
    pub model: Option<String>,
    /// Its instructions and playbook: the first of what a run of it is told.
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<Mcp>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub limits: Limits,
    /// The Agentfile its microVM is made from, as a start names one: one of its owner's
    /// (`agentfile:<id>`) or kiln's for its harness (`harness-<harness>`); none: its harness's own.
    #[serde(default)]
    pub agentfile: Option<String>,
    /// What it keeps from one of its sessions to the next on each machine ([`volumes`], P6.2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<volumes::Volume>,
    /// Where its sessions run: `host`, its harness a process of its machine's; none, a microVM of its own made from
    /// its Agentfile, as every definition ran before it had one. What an experiment's promoted variant makes it
    /// (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1, [`experiments`]), as are its size's two below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
    /// Its microVM's vCPUs; none: as kiln sizes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    /// Its microVM's memory, in bytes, a whole number of MiB; none: as kiln sizes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
}

impl Settings {
    /// The Agentfile a microVM of it is made from: its own, else kiln's for its harness where kiln ships one.
    pub fn microvm_agentfile(&self) -> Option<String> {
        self.agentfile.clone().or_else(|| crate::agentfiles::builtin::ALL.iter().find(|b| b.harness == self.harness.trim()).map(|b| b.name.to_owned()))
    }

    /// The Agentfile its sessions' microVMs are made from ([`Settings::microvm_agentfile`]); none on the host.
    pub fn agentfile_in_use(&self) -> Option<String> {
        if self.engine == Some(Engine::Host) { None } else { self.microvm_agentfile() }
    }
}

/// An agent definition, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentDef {
    pub id: String,
    /// Whose it is: `owner` (the server's), or `user:<id>`.
    pub owner: String,
    /// Its name, once among its owner's.
    pub name: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(flatten)]
    pub settings: Settings,
}

/// An agent definition as a list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Brief {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub harness: String,
    pub model: Option<String>,
    pub agentfile: Option<String>,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One version of an agent definition's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Version {
    pub version: i64,
    pub at: i64,
    /// Who made it, as the audit trail names them.
    pub by: String,
    #[serde(flatten)]
    pub settings: Settings,
}

/// A failure the caller can act on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Bad(String),
    #[error("no such agent definition")]
    Missing,
    /// Its owner has another of that name.
    #[error("{0}")]
    Conflict(String),
    /// A change made over version `base`, which isn't current: another change came first. What it is
    /// now (`current`) goes with the refusal, so the caller can see what changed in between and make
    /// theirs over it.
    #[error("it was changed since version {base}, which this change was made over: it's version {} now. Read it again and make your change over that (`base` {})", .current.version, .current.version)]
    Stale { base: i64, current: Box<AgentDef> },
    /// Its settings aren't an agent kiln runs: each reason.
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    #[error("{0}")]
    Forbidden(String),
    /// Kiln's own ([`builtin`]): read only.
    #[error("{0} is kiln's own agent definition: copy it as yours (agentdefs.copy) to change it")]
    ReadOnly(String),
    /// What it names couldn't be read just now (an Agentfile's record).
    #[error("{0}")]
    Unavailable(String),
    /// It and its Agentfile's `AGENT` each changed a setting they share differently since they last
    /// agreed (kiln plan 01a1dd2e… P5.4): neither is changed.
    #[error("{}", sync::words(.0))]
    Diverged(Vec<sync::Diverged>),
    /// One of a person's agent volumes on a machine refused a clearing by `cause` (kiln plan 01a1dd2e…, P6.2):
    /// [`volumes::clear`].
    #[error("{why}")]
    Volume { cause: volumes::Cause, why: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

type R<T> = Result<T, Error>;

/// Who asks: whose agent definitions are theirs, and whether they reach everyone's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    /// `owner`, or `user:<id>`: an agent's, the person it acts for.
    pub person: String,
    /// The server's owner, or a person with `agentdefs.manage`; never an agent.
    pub all: bool,
}

impl Who {
    pub fn of(caller: &Caller) -> R<Who> {
        let person = crate::harness::lineage::person_of(caller).ok_or_else(|| Error::Forbidden(format!("{} acts for no person here, and an agent definition is a person's", caller.name)))?;
        Ok(Who { person, all: caller.agent.is_none() && (caller.is_owner() || caller.can(MANAGE, &Scope::Server)) })
    }

    pub(crate) fn reaches(&self, owner: &str) -> bool {
        self.all || self.person == owner
    }
}

/// What's wrong with `name` as an agent's, if anything: it's an Agentfile `AGENT`'s too, so held to
/// an image's names (lowercase letters and digits joined by `.`, `_` or `-`).
pub fn name_problem(name: &str) -> Option<String> {
    crate::agentfiles::name_problem(name).map(|_| format!("`{name}` isn't an agent's name: lowercase letters and digits joined by `.`, `_` or `-` (`reviewer`), 255 at the most"))
}

/// Why `s`, `owner`'s, isn't an agent kiln runs: every reason, none when it is. It names one of
/// kiln's harnesses; its Agentfile is one its owner reaches, for that harness; its model is one kiln's
/// machines list for that harness, where they list any (what its sessions said they offer); its MCP
/// servers each have a name of their own and one way in; its tools and skills are named; a limit
/// it sets lets it do something, a token limit is set only for a harness that says how many tokens
/// its sessions use (`Kind::says_tokens`) and a cost limit only for one that says what they cost
/// (`Kind::says_cost`); and its volumes each have a name of their own and a bound
/// ([`volumes::problems`]).
pub async fn check(hub: &Hub, owner: &str, s: &Settings) -> R<Vec<String>> {
    let mut why = Vec::new();
    let Some(kind) = Kind::get(s.harness.trim()) else {
        let ten: Vec<&str> = Kind::all().iter().map(|k| k.id).collect();
        why.push(format!("`{}` isn't one of kiln's harnesses: {}", s.harness, ten.join(", ")));
        return Ok(why);
    };
    if let Some(asked) = s.agentfile.as_deref() {
        let who = crate::agentfiles::Who { person: owner.to_owned(), all: false };
        match crate::harness::lineage::image_for(hub, &who, kind, asked).await {
            Ok(_) => {}
            Err((code, said)) if code == http::StatusCode::SERVICE_UNAVAILABLE => return Err(Error::Unavailable(said)),
            Err((_, said)) => why.push(said),
        }
    }
    if let Some(model) = s.model.as_deref().map(str::trim) {
        let listed = models(hub, kind.id);
        if model.is_empty() {
            why.push("`model` names a model, or is left out for its harness's default".into());
        } else if !listed.is_empty() && !listed.iter().any(|m| m == model) {
            why.push(format!("{} takes no model `{model}` that kiln's machines list: {}", kind.name, listed.join(", ")));
        }
    }
    let mut names: Vec<&str> = Vec::new();
    for m in &s.mcp {
        let name = m.name.trim();
        if name.is_empty() {
            why.push("an MCP server needs its name".into());
        } else if names.contains(&name) {
            why.push(format!("two MCP servers are named {name}"));
        } else {
            names.push(name);
        }
        let by_url = m.url.as_deref().is_some_and(|u| !u.trim().is_empty());
        let by_command = m.command.first().is_some_and(|c| !c.trim().is_empty());
        if by_url == by_command {
            why.push(format!("MCP server {name} is reached by its `url` or started by its `command`: one of them"));
        }
    }
    if s.tools.iter().any(|t| t.trim().is_empty()) {
        why.push("a tool needs its name".into());
    }
    if s.skills.iter().any(|t| t.trim().is_empty()) {
        why.push("a skill needs its name".into());
    }
    for (limit, v) in [("seconds", s.limits.seconds), ("cents", s.limits.cents), ("tokens", s.limits.tokens)] {
        if v == Some(0) {
            why.push(format!("a limit of 0 {limit} lets it do nothing: leave it out for none"));
        }
    }
    if s.limits.tokens.is_some() && !kind.says_tokens() {
        why.push(format!("{} doesn't say how many tokens its sessions use, so kiln can't hold a limit in `tokens` to it: limit its `seconds` instead", kind.name));
    }
    if s.limits.cents.is_some() && !kind.says_cost() {
        // Its tokens where it says them, else its time, which kiln holds to every harness.
        let instead = if kind.says_tokens() { "tokens" } else { "seconds" };
        why.push(format!("{} doesn't say what its sessions cost, so kiln can't hold a limit in `cents` to it: limit its `{instead}` instead", kind.name));
    }
    why.extend(volumes::problems(&s.volumes));
    if s.engine == Some(Engine::Host) && (s.agentfile.is_some() || s.cpus.is_some() || s.memory.is_some()) {
        why.push("an Agentfile and a size are a microVM's: on the host engine its harness runs as a process of its machine".into());
    }
    // A start on its machine's host is refused where it has volumes (P6.2, `crate::harness::lineage`'s start): so is
    // a definition, or an experiment's variant (P7.1), on the host engine with them.
    if s.engine == Some(Engine::Host) && !s.volumes.is_empty() {
        why.push("its volumes are kept only for its sessions in microVMs: on the host engine its harness runs as a process of its machine".into());
    }
    if s.cpus == Some(0) {
        why.push("a microVM of 0 vCPUs runs nothing: leave `cpus` out for kiln to size it".into());
    }
    match s.memory {
        Some(0) => why.push("a microVM of no memory runs nothing: leave `memory` out for kiln to size it".into()),
        Some(m) if m % MIB != 0 => why.push(format!("`memory` is in bytes, a whole number of MiB ({MIB} bytes each), not {m}")),
        _ => {}
    }
    Ok(why)
}

/// The models harness `kind` takes as kiln's machines list them (each machine's from what its
/// sessions said they offer), each once: none when no machine lists any.
fn models(hub: &Hub, kind: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in hub.harness.kinds().into_values().flatten().filter(|k| k.id == kind) {
        for m in k.models {
            if !out.contains(&m.id) {
                out.push(m.id);
            }
        }
    }
    out
}

/// `s` checked for `owner`: refused, every reason, when it isn't an agent kiln runs.
async fn checked(hub: &Hub, owner: &str, s: Settings) -> R<Settings> {
    let why = check(hub, owner, &s).await?;
    if why.is_empty() { Ok(tidy(s)) } else { Err(Error::Refused(why)) }
}

/// `s` as kept: its names and model trimmed, and a microVM, which a definition runs in by default, said as none.
fn tidy(mut s: Settings) -> Settings {
    s.harness = s.harness.trim().to_owned();
    s.model = s.model.map(|m| m.trim().to_owned());
    s.agentfile = s.agentfile.map(|a| a.trim().to_owned());
    s.engine = s.engine.filter(|e| *e != Engine::Microvm);
    s
}

#[derive(Debug, Clone)]
struct Record {
    id: String,
    owner: String,
    name: String,
    version: i64,
    settings: String,
    created_at: i64,
    updated_at: i64,
}

impl Record {
    fn def(self) -> R<AgentDef> {
        let settings = read(&self.settings)?;
        Ok(AgentDef { id: self.id, owner: self.owner, name: self.name, version: self.version, created_at: self.created_at, updated_at: self.updated_at, settings })
    }
}

fn read(json: &str) -> R<Settings> {
    serde_json::from_str(json).map_err(|e| Error::Other(anyhow::anyhow!("kiln.db holds an agent definition's settings it can't read: {e}")))
}

fn write(s: &Settings) -> R<String> {
    serde_json::to_string(s).map_err(|e| Error::Other(e.into()))
}

const COLUMNS: &str = "id, owner, name, version, settings, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record { id: r.get(0)?, owner: r.get(1)?, name: r.get(2)?, version: r.get(3)?, settings: r.get(4)?, created_at: r.get(5)?, updated_at: r.get(6)? })
}

fn get_in(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Record>> {
    c.query_row(&format!("SELECT {COLUMNS} FROM agent_defs WHERE id = ?1"), params![id], row).optional()
}

/// Whether `owner` has an agent definition named `name` other than `id`.
fn named_in(c: &rusqlite::Connection, owner: &str, name: &str, id: &str) -> rusqlite::Result<bool> {
    c.query_row("SELECT EXISTS(SELECT 1 FROM agent_defs WHERE owner = ?1 AND name = ?2 AND id <> ?3)", params![owner, name, id], |r| r.get(0))
}

fn taken(name: &str) -> Error {
    Error::Conflict(format!("you have an agent named {name} already: give this one another name"))
}

/// The caller's own, or with `everyone` everyone's, newest change first.
pub async fn list(hub: &Hub, who: &Who, everyone: bool) -> R<Vec<Brief>> {
    if everyone && !who.all {
        return Err(Error::Forbidden(format!("everyone's agent definitions are for the server's owner and those with {MANAGE}")));
    }
    let owner = (!everyone).then(|| who.person.clone());
    let rows: Vec<Record> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM agent_defs WHERE ?1 IS NULL OR owner = ?1 ORDER BY updated_at DESC, id"))?;
            let rows = st.query_map(params![owner], row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    rows.into_iter()
        .map(|r| {
            let d = r.def()?;
            Ok(Brief { id: d.id, owner: d.owner, name: d.name, harness: d.settings.harness, model: d.settings.model, agentfile: d.settings.agentfile, version: d.version, created_at: d.created_at, updated_at: d.updated_at })
        })
        .collect()
}

/// The record of one the caller reaches; another's is as none, and kiln's own ([`builtin`]) are
/// read only.
async fn record(hub: &Hub, who: &Who, id: &str) -> R<Record> {
    if id.starts_with(builtin::PREFIX) {
        return Err(Error::ReadOnly(id.to_owned()));
    }
    let id = id.to_owned();
    let r = hub.store.read(move |c| Ok(get_in(c, &id)?)).await?;
    r.filter(|r| who.reaches(&r.owner)).ok_or(Error::Missing)
}

/// One the caller reaches; another's is as none.
pub async fn get(hub: &Hub, who: &Who, id: &str) -> R<AgentDef> {
    record(hub, who, id).await?.def()
}

/// The settings and name of one the caller may run or copy: one of theirs, or kiln's own.
pub async fn settings_of(hub: &Hub, who: &Who, id: &str) -> R<(String, i64, Settings)> {
    if let Some(b) = builtin::get(id) {
        return Ok((b.name, 0, b.settings));
    }
    let d = get(hub, who, id).await?;
    Ok((d.name, d.version, d.settings))
}

/// One the caller may run or copy (theirs, or kiln's own: [`settings_of`]) made one of theirs, named
/// `name` (by default as it is): version 1, checked as [`create`] checks it.
pub async fn copy(hub: &Hub, who: &Who, id: &str, name: Option<&str>, by: &str) -> R<AgentDef> {
    let (was, _, settings) = settings_of(hub, who, id).await?;
    sync::create(hub, who, name.unwrap_or(&was), settings, by).await
}

/// Its versions, newest first.
pub async fn versions(hub: &Hub, who: &Who, id: &str) -> R<Vec<Version>> {
    record(hub, who, id).await?;
    let id = id.to_owned();
    let rows: Vec<(i64, String, i64, String)> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare("SELECT version, settings, at, by FROM agent_def_versions WHERE agent_def_id = ?1 ORDER BY version DESC")?;
            let rows = st.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    rows.into_iter().map(|(version, s, at, by)| Ok(Version { version, at, by, settings: read(&s)? })).collect()
}

/// A new agent definition of the caller's, once its settings pass [`check`]: version 1.
pub async fn create(hub: &Hub, who: &Who, name: &str, settings: Settings, by: &str) -> R<AgentDef> {
    let name = name.trim().to_owned();
    if let Some(p) = name_problem(&name) {
        return Err(Error::Bad(p));
    }
    let settings = checked(hub, &who.person, settings).await?;
    let json = write(&settings)?;
    let (id, owner, now, by, n) = (crate::util::new_id(), who.person.clone(), crate::util::now_unix(), by.to_owned(), name.clone());
    let made = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            if named_in(&tx, &owner, &n, &id)? {
                return Ok(None);
            }
            tx.execute("INSERT INTO agent_defs (id, owner, name, version, settings, created_at, updated_at) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?5)", params![id, owner, n, json, now])?;
            tx.execute("INSERT INTO agent_def_versions (agent_def_id, version, settings, at, by) VALUES (?1, 1, ?2, ?3, ?4)", params![id, json, now, by])?;
            let r = get_in(&tx, &id)?;
            tx.commit()?;
            Ok(r)
        })
        .await?;
    made.ok_or_else(|| taken(&name))?.def()
}

/// A change: what's given changes. New settings are a new version, once they pass [`check`]; a new
/// name alone is none.
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub name: Option<String>,
    pub harness: Option<String>,
    /// `Some(None)`: back to its harness's default.
    pub model: Option<Option<String>>,
    pub instructions: Option<String>,
    pub tools: Option<Vec<String>>,
    pub mcp: Option<Vec<Mcp>>,
    pub skills: Option<Vec<String>>,
    pub limits: Option<Limits>,
    /// `Some(None)`: back to its harness's own.
    pub agentfile: Option<Option<String>>,
    pub volumes: Option<Vec<volumes::Volume>>,
    /// `Some(None)`: back to a microVM of its own.
    pub engine: Option<Option<Engine>>,
    /// `Some(None)`: back to kiln's sizing, as the next two.
    pub cpus: Option<Option<u32>>,
    pub memory: Option<Option<u64>>,
}

impl Patch {
    fn over(self, mut s: Settings) -> Settings {
        if let Some(v) = self.harness {
            s.harness = v;
        }
        if let Some(v) = self.model {
            s.model = v;
        }
        if let Some(v) = self.instructions {
            s.instructions = v;
        }
        if let Some(v) = self.tools {
            s.tools = v;
        }
        if let Some(v) = self.mcp {
            s.mcp = v;
        }
        if let Some(v) = self.skills {
            s.skills = v;
        }
        if let Some(v) = self.limits {
            s.limits = v;
        }
        if let Some(v) = self.agentfile {
            s.agentfile = v;
        }
        if let Some(v) = self.volumes {
            s.volumes = v;
        }
        if let Some(v) = self.engine {
            s.engine = v;
        }
        if let Some(v) = self.cpus {
            s.cpus = v;
        }
        if let Some(v) = self.memory {
            s.memory = v;
        }
        s
    }
}

/// What a change's write found.
enum Written {
    Done(Record),
    Missing,
    Taken,
    /// Another change came first: what it is now.
    Moved(Record),
}

/// Agent definition `id` changed as `patch` says, by `by`, over version `base` where it names one
/// (kiln plan 01a1dd2e… UI contract, principle 5: a save carries its base, and one made over a version
/// another save has moved on from is refused, never a silent overwrite), else over the version it
/// finds. Either way the version it's made over is held to in the write itself: another change in
/// between refuses it ([`Error::Stale`]) rather than being overwritten. A rename alone is no version,
/// so renames aren't held to each other.
pub async fn update(hub: &Hub, who: &Who, id: &str, mut patch: Patch, base: Option<i64>, by: &str) -> R<AgentDef> {
    let old = record(hub, who, id).await?;
    if let Some(base) = base.filter(|b| *b != old.version) {
        return Err(Error::Stale { base, current: Box::new(old.def()?) });
    }
    let name = patch.name.take().map_or_else(|| old.name.clone(), |n| n.trim().to_owned());
    if let Some(p) = name_problem(&name) {
        return Err(Error::Bad(p));
    }
    let was = read(&old.settings)?;
    let wanted = patch.over(was.clone());
    let settings = if wanted == was { was.clone() } else { checked(hub, &old.owner, wanted).await? };
    let new_version = settings != was;
    if name == old.name && !new_version {
        return old.def();
    }
    let json = write(&settings)?;
    let (id, now, by, n, at) = (id.to_owned(), crate::util::now_unix(), by.to_owned(), name.clone(), old.version);
    let written = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(now_is) = get_in(&tx, &id)? else { return Ok(Written::Missing) };
            // What it was when this change began: another since is its own change, not overwritten.
            if now_is.version != at {
                return Ok(Written::Moved(now_is));
            }
            if named_in(&tx, &now_is.owner, &n, &id)? {
                return Ok(Written::Taken);
            }
            let version = if new_version { at.saturating_add(1) } else { at };
            tx.execute("UPDATE agent_defs SET name = ?2, settings = ?3, version = ?4, updated_at = ?5 WHERE id = ?1", params![id, n, json, version, now])?;
            if new_version {
                tx.execute("INSERT INTO agent_def_versions (agent_def_id, version, settings, at, by) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, version, json, now, by])?;
            }
            let r = get_in(&tx, &id)?;
            tx.commit()?;
            Ok(r.map_or(Written::Missing, Written::Done))
        })
        .await?;
    match written {
        Written::Done(r) => r.def(),
        Written::Missing => Err(Error::Missing),
        Written::Taken => Err(taken(&name)),
        Written::Moved(now_is) => Err(Error::Stale { base: at, current: Box::new(now_is.def()?) }),
    }
}

/// Version `version`'s settings made current again, as a new version, so nothing is lost: checked
/// again, as what they name may have gone since.
pub async fn restore(hub: &Hub, who: &Who, id: &str, version: i64, by: &str) -> R<AgentDef> {
    let d = record(hub, who, id).await?;
    let i = id.to_owned();
    let then: Option<String> = hub.store.read(move |c| Ok(c.query_row("SELECT settings FROM agent_def_versions WHERE agent_def_id = ?1 AND version = ?2", params![i, version], |r| r.get(0)).optional()?)).await?;
    let then = read(&then.ok_or_else(|| Error::Bad(format!("the agent definition has no version {version}")))?)?;
    if then == read(&d.settings)? {
        // As it is already: nothing to make again.
        return d.def();
    }
    let s = then;
    let patch = Patch {
        harness: Some(s.harness),
        model: Some(s.model),
        instructions: Some(s.instructions),
        tools: Some(s.tools),
        mcp: Some(s.mcp),
        skills: Some(s.skills),
        limits: Some(s.limits),
        agentfile: Some(s.agentfile),
        volumes: Some(s.volumes),
        engine: Some(s.engine),
        cpus: Some(s.cpus),
        memory: Some(s.memory),
        name: None,
    };
    sync::update(hub, who, id, patch, None, by).await
}

/// Agent definition `id` and its versions, gone: what it was.
pub async fn delete(hub: &Hub, who: &Who, id: &str) -> R<AgentDef> {
    let d = record(hub, who, id).await?;
    let i = id.to_owned();
    let n = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let n = tx.execute("DELETE FROM agent_defs WHERE id = ?1", params![i])?;
            tx.execute("DELETE FROM agent_def_versions WHERE agent_def_id = ?1", params![i])?;
            // Where it stood in for one of kiln's agents (`uses`), kiln's own again.
            tx.execute("DELETE FROM agent_def_uses WHERE agent_def_id = ?1", params![i])?;
            // The sessions it started no longer say it (`sessions`).
            sessions::forget_in(&tx, &i)?;
            // Its experiments go with it (`experiments`).
            experiments::forget_in(&tx, &i)?;
            tx.commit()?;
            Ok(n)
        })
        .await?;
    if n == 0 {
        return Err(Error::Missing);
    }
    d.def()
}

/// What a start of `s` sizes its microVM by, as a start's size takes it (its vCPUs and MiB, either 0 for kiln's own
/// sizing of it): none where it says neither, or runs on the host.
pub fn size_of(s: &Settings) -> Option<(u32, u64)> {
    if s.engine == Some(Engine::Host) || (s.cpus.is_none() && s.memory.is_none()) {
        return None;
    }
    Some((s.cpus.unwrap_or(0), s.memory.map_or(0, |m| m / MIB)))
}

/// What a run of `d` is first told: its instructions, then the task, each where there's one.
pub fn first_message(d: &Settings, task: Option<&str>) -> Option<String> {
    let parts: Vec<&str> = [Some(d.instructions.as_str()), task].into_iter().flatten().map(str::trim).filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

#[cfg(test)]
mod tests;
