//! Environments for agent sessions (docs/agents.md, "Environments"; decided by the user, 2026-11-18): a
//! person's named sets of variables an agent session runs with, each picked from the machine's user's,
//! set to a value, given one of the person's own secrets (`crate::secrets::mine`), or taken away. Named
//! `agentenv` in kiln's operations, routes, wire and tables, apart from CI's and CD's environments
//! (docs/ci.md W15).
//!
//! - **Theirs alone.** Each is its owner's (`owner`: the server's `owner`, or `user:<id>`), named once
//!   among theirs. A person reaches only their own, an admin included, as with their secrets: a `value`
//!   variable may hold one (the person's choice, never looked into). An agent keeps, changes and applies
//!   none; the agents it starts run with its own, or one of its person's its start names.
//! - **Kept as written.** kiln.db keeps its variables as the person wrote them, a secret by its name
//!   ([`crate::harness::agentenv::Var`]); its version goes up with each change.
//! - **Opened as a harness starts** ([`resolve`]): each `secret` read from its owner's own, marked used and
//!   audited by name, into what the session's runner gets ([`crate::harness::agentenv::Resolved`]), and
//!   opened again (its newest version, its secrets read again) whenever that session's harness starts:
//!   started, restarted, resumed from kiln's copy, moved or taken over by another runner process.
//! - **Which each session runs with** is kept by the session's id (`agentenv_sessions`), and here in
//!   memory ([`Held`]) for the updates that ask, so one some session runs with can't be deleted.
//!
//! The same operations serve the console (`/api/agentenvs`, `PUT /api/harness/sessions/:id/agentenv`),
//! kiln's MCP server and code mode ([`ops`]).

pub mod ops;
pub mod resolve;

use std::collections::HashMap;

use http::StatusCode;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use crate::harness::agentenv::{Base, From as VarFrom, Var};
use crate::hub::Hub;
use crate::wire::events::{AgentenvChange, ServerEvent};

/// An environment, as kept and shown: its variables as its owner wrote them, never a secret's value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Agentenv {
    pub id: String,
    /// Whose it is: `owner` (the server's), or `user:<id>`.
    pub owner: String,
    /// Its name, once among its owner's.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub description: Option<String>,
    pub base: Base,
    /// In order: a later one of a name wins.
    pub vars: Vec<Var>,
    /// Its owner's own labels for it, in the order given: found by the Engine tab's filter.
    pub tags: Vec<String>,
    /// Up by one with each change.
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// The sessions that run with it, by id.
    pub sessions: Vec<String>,
}

/// The caller's environments (`agentenvs.list`), by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentenvListing"))]
pub struct Listing {
    pub agentenvs: Vec<Agentenv>,
}

/// The environment a session runs with, as its info says it (`AgentInfo::agentenv`): the version it
/// started with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentenvBrief"))]
pub struct Brief {
    pub id: String,
    pub name: String,
    pub version: u64,
}

impl Brief {
    /// What a session given `r` runs with.
    pub fn of(r: &crate::harness::agentenv::Resolved) -> Brief {
        Brief { id: r.id.clone(), name: r.name.clone(), version: r.version }
    }
}

/// What `agentenvs.apply` did: the session, the environment it runs with now (none: its machine's user's),
/// and whether its harness was started again on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentenvApplied"))]
pub struct Applied {
    pub agent: String,
    pub agentenv: Option<Brief>,
    pub restarted: bool,
}

/// Why an environment's operation, or a start or a restart with one, is refused: each with the code the
/// UI reads ([`Refusal::code`]) where docs/agents.md names one, and words it shows.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// None of the caller's has that id.
    #[error("{0}")]
    Unknown(String),
    #[error("{0}")]
    NameTaken(String),
    /// Changed since the version the change was made over: the version it's at now.
    #[error("it was changed since version {asked}, which this change was made over: it's version {now} now. Read it again and make your change over that")]
    Stale { asked: i64, now: i64 },
    /// Sessions run with it: their ids.
    #[error("sessions run with it: {}. Apply another environment to each (or none) first", .0.join(", "))]
    InUse(Vec<String>),
    /// Secrets it names that its owner holds none of: their names.
    #[error("it names {}, which isn't one of your secrets: set it at /api/me/secrets (secrets.mine.set), or take the variable out", .0.join(", "))]
    SecretMissing(Vec<String>),
    /// Its `secret` variables, by name, for a session in a microVM.
    #[error("its {} variables are secrets, and a session in a microVM is given a secret by name, never by value: take them out of it, or start it on its machine's host", .0.join(", "))]
    SecretInMicrovm(Vec<String>),
    #[error("{0}")]
    BadVar(String),
    /// Its runner can't run sessions with one.
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type R<T> = Result<T, Refusal>;

/// The codes docs/agents.md names, as a refusal's words start with them ([`Refusal::said`], [`typed`]).
pub const CODES: &[&str] = &["agentenv_unknown", "agentenv_name_taken", "agentenv_stale", "agentenv_in_use", "agentenv_secret_missing", "agentenv_secret_in_microvm", "agentenv_bad_var"];

impl Refusal {
    /// Its code (docs/agents.md, "Operations"), where it has one.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Unknown(_) => Some("agentenv_unknown"),
            Self::NameTaken(_) => Some("agentenv_name_taken"),
            Self::Stale { .. } => Some("agentenv_stale"),
            Self::InUse(_) => Some("agentenv_in_use"),
            Self::SecretMissing(_) => Some("agentenv_secret_missing"),
            Self::SecretInMicrovm(_) => Some("agentenv_secret_in_microvm"),
            Self::BadVar(_) => Some("agentenv_bad_var"),
            Self::Unsupported(_) | Self::Forbidden(_) | Self::Other(_) => None,
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::Unknown(_) => StatusCode::NOT_FOUND,
            Self::NameTaken(_) | Self::Stale { .. } | Self::InUse(_) | Self::SecretInMicrovm(_) | Self::Unsupported(_) => StatusCode::CONFLICT,
            Self::SecretMissing(_) | Self::BadVar(_) => StatusCode::BAD_REQUEST,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Its words as every surface carries them (an operation's failure is a status and words): its code
    /// first, where it has one (`agentenv_unknown: …`), which the console's routes answer apart ([`typed`]).
    pub fn said(&self) -> String {
        match self.code() {
            Some(code) => format!("{code}: {self}"),
            None => format!("{self:#}"),
        }
    }

    /// As an operation fails.
    pub fn fail(&self) -> crate::ops::Fail {
        (self.status(), self.said())
    }
}

/// An operation's failure words as [`Refusal::said`] wrote them: its code and its words apart, where they
/// start with one of [`CODES`].
pub fn typed(said: &str) -> Option<(&'static str, &str)> {
    let (code, words) = said.split_once(": ")?;
    CODES.iter().find(|c| **c == code).map(|c| (*c, words))
}

/// Whose environments the caller's are: the person it is. An agent keeps, changes and applies none: a person
/// chooses the sessions their secrets go to (`crate::secrets::mine`: an agent sets none).
pub fn person(caller: &crate::authz::Caller) -> R<String> {
    if caller.agent.is_some() || caller.kind == crate::authz::gate::Kind::Agent {
        return Err(Refusal::Forbidden("a person keeps and applies their own environments: an agent doesn't".into()));
    }
    crate::harness::lineage::person_of(caller).ok_or_else(|| Refusal::Forbidden(format!("{} acts for no person here, and an environment is a person's", caller.name)))
}

/// Whether `caller` hears of `owner`'s environments changing (`ServerEvent::Agentenv`): its own alone.
pub fn hears(caller: &crate::authz::Caller, owner: &str) -> bool {
    person(caller).is_ok_and(|p| p == owner)
}

/// An environment as its owner asks for it: what `agentenvs.create` takes, and what a change makes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub name: String,
    pub description: Option<String>,
    pub base: Base,
    pub vars: Vec<Var>,
    pub tags: Vec<String>,
}

/// A change: what's given changes (`description: Some(None)` takes it out).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub base: Option<Base>,
    pub vars: Option<Vec<Var>>,
    pub tags: Option<Vec<String>>,
}

/// `tags` as kept: each trimmed, the empty dropped, each once (the first of those that differ only in
/// case); refused, saying which, where one holds a control character (a line break among them).
fn tags_of(tags: Vec<String>) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags.into_iter().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()) {
        if t.chars().any(char::is_control) {
            return Err(format!("the tag {t:?} has a control character"));
        }
        if !out.iter().any(|o| o.to_lowercase() == t.to_lowercase()) {
            out.push(t);
        }
    }
    Ok(out)
}

/// What's wrong with `name` as an environment's, if anything: some words, on one line.
fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("an environment needs a name".into());
    }
    name.chars().any(char::is_control).then(|| "an environment's name is words on one line, without control characters".into())
}

/// What's wrong with `vars`, if anything ([`Refusal::BadVar`]): each a portable name
/// (`crate::harness::agentenv::valid_name`) with what its `from` takes and nothing else.
pub fn var_problem(vars: &[Var]) -> Option<String> {
    for v in vars {
        let n = &v.name;
        if !crate::harness::agentenv::valid_name(n) {
            return Some(format!("`{n}` isn't a variable's name: letters, digits and `_`, not starting with a digit"));
        }
        let (of, value, secret) = (v.of.is_some(), v.value.is_some(), v.secret.is_some());
        let wrong = match v.from {
            VarFrom::Machine if value || secret => Some("a `machine` variable takes `of` alone (the machine's variable it takes, when not its own name)"),
            VarFrom::Machine => v.of.as_deref().filter(|o| !crate::harness::agentenv::valid_name(o)).map(|_| "its `of` isn't a variable's name"),
            VarFrom::Value if of || secret => Some("a `value` variable takes `value` alone"),
            VarFrom::Value if !value => Some("a `value` variable needs its `value`"),
            VarFrom::Value => None,
            VarFrom::Secret if of || value => Some("a `secret` variable takes `secret` alone: one of your secrets by its name, never its value"),
            VarFrom::Secret => match v.secret.as_deref().map(kiln_ci::secrets::check_name) {
                Some(Ok(_)) => None,
                _ => Some("its `secret` names one of your secrets: letters, digits and `_`, not starting with a digit"),
            },
            VarFrom::Unset if of || value || secret => Some("an `unset` variable takes nothing but its name"),
            VarFrom::Unset => None,
        };
        if let Some(why) = wrong {
            return Some(format!("{n}: {why}"));
        }
    }
    None
}

/// The secrets `vars` names, by their names as kept (upper-case), each once.
pub fn secrets_of(vars: &[Var]) -> Vec<String> {
    let mut names: Vec<String> = vars.iter().filter(|v| v.from == VarFrom::Secret).filter_map(|v| v.secret.as_deref()).filter_map(|s| kiln_ci::secrets::check_name(s).ok()).collect();
    let mut seen = std::collections::HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    names
}

/// The variables of `vars` that are secrets, by their own names.
pub fn secret_vars(vars: &[Var]) -> Vec<String> {
    vars.iter().filter(|v| v.from == VarFrom::Secret).map(|v| v.name.clone()).collect()
}

/// Of the secrets `vars` names, those `person` holds none of.
async fn missing(hub: &Hub, person: &str, vars: &[Var]) -> R<Vec<String>> {
    let named = secrets_of(vars);
    if named.is_empty() {
        return Ok(Vec::new());
    }
    let kept = crate::ci::secrets::list(hub, &crate::ci::secrets::user_scope(person)).await?;
    Ok(named.into_iter().filter(|n| !kept.iter().any(|k| k.name.eq_ignore_ascii_case(n))).collect())
}

/// `vars` as a person may keep them: each well formed, each secret one of theirs.
async fn checked(hub: &Hub, person: &str, vars: &[Var]) -> R<()> {
    if let Some(why) = var_problem(vars) {
        return Err(Refusal::BadVar(why));
    }
    let missing = missing(hub, person, vars).await?;
    if missing.is_empty() { Ok(()) } else { Err(Refusal::SecretMissing(missing)) }
}

/// What kiln.db keeps of one.
#[derive(Debug, Clone)]
struct Record {
    id: String,
    owner: String,
    name: String,
    description: Option<String>,
    base: String,
    vars: String,
    tags: String,
    version: i64,
    created_at: i64,
    updated_at: i64,
}

const COLUMNS: &str = "id, owner, name, description, base, vars, tags, version, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record { id: r.get(0)?, owner: r.get(1)?, name: r.get(2)?, description: r.get(3)?, base: r.get(4)?, vars: r.get(5)?, tags: r.get(6)?, version: r.get(7)?, created_at: r.get(8)?, updated_at: r.get(9)? })
}

impl Record {
    /// As shown, with the sessions that run with it.
    fn read(self, sessions: Vec<String>) -> anyhow::Result<Agentenv> {
        let base: Base = serde_json::from_value(serde_json::Value::String(self.base.clone())).map_err(|e| anyhow::anyhow!("environment {}'s base {:?} isn't one: {e}", self.id, self.base))?;
        let vars: Vec<Var> = serde_json::from_str(&self.vars).map_err(|e| anyhow::anyhow!("environment {}'s variables can't be read: {e}", self.id))?;
        let tags: Vec<String> = serde_json::from_str(&self.tags).map_err(|e| anyhow::anyhow!("environment {}'s tags can't be read: {e}", self.id))?;
        Ok(Agentenv { id: self.id, owner: self.owner, name: self.name, description: self.description, base, vars, tags, version: self.version, created_at: self.created_at, updated_at: self.updated_at, sessions })
    }
}

fn base_text(b: Base) -> &'static str {
    match b {
        Base::User => "user",
        Base::Clean => "clean",
    }
}

fn get_in(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Record>> {
    c.query_row(&format!("SELECT {COLUMNS} FROM agentenvs WHERE id = ?1"), params![id], row).optional()
}

/// Whether `owner` has an environment named `name` other than `id`.
fn named_in(c: &rusqlite::Connection, owner: &str, name: &str, id: &str) -> rusqlite::Result<bool> {
    c.query_row("SELECT EXISTS(SELECT 1 FROM agentenvs WHERE owner = ?1 AND name = ?2 AND id <> ?3)", params![owner, name, id], |r| r.get(0))
}

fn taken(name: &str) -> Refusal {
    Refusal::NameTaken(format!("you have an environment named {name} already: give this one another name"))
}

fn unknown(id: &str) -> Refusal {
    Refusal::Unknown(format!("you have no environment {id}"))
}

/// The sessions kiln runs that run with environment `id`, by id (a session it no longer runs isn't one).
pub fn sessions_of(hub: &Hub, id: &str) -> Vec<String> {
    let mut out: Vec<String> = hub.agentenvs.links.load().iter().filter(|(s, e)| *e == id && hub.harness.location(s).is_some()).map(|(s, _)| s.clone()).collect();
    out.sort();
    out
}

/// Tells its owner's events sockets an environment changed or went, or which sessions run with it did.
fn told(hub: &Hub, owner: &str, id: &str, deleted: bool) {
    hub.emit(&ServerEvent::Agentenv(AgentenvChange { owner: owner.to_owned(), agentenv: id.to_owned(), deleted }));
}

/// `person`'s own, by name.
pub async fn list(hub: &Hub, person: &str) -> R<Vec<Agentenv>> {
    let p = person.to_owned();
    let rows: Vec<Record> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM agentenvs WHERE owner = ?1 ORDER BY name, id"))?;
            let rows = st.query_map(params![p], row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    rows.into_iter()
        .map(|r| {
            let s = sessions_of(hub, &r.id);
            r.read(s).map_err(Refusal::from)
        })
        .collect()
}

/// One of `person`'s own; another's is as none.
pub async fn get(hub: &Hub, person: &str, id: &str) -> R<Agentenv> {
    let i = id.to_owned();
    let found = hub.store.read(move |c| Ok(get_in(c, &i)?)).await?;
    match found.filter(|r| r.owner == person) {
        Some(r) => Ok(r.read(sessions_of(hub, id))?),
        None => Err(unknown(id)),
    }
}

/// The environment session `session` runs with, whoever's: none, its machine's user's.
pub async fn of_session(hub: &Hub, session: &str) -> R<Option<Agentenv>> {
    match linked(hub, session) {
        Some(id) => any(hub, &id).await,
        None => Ok(None),
    }
}

/// Environment `id`, whoever's: the one a session runs with, opened as its owner's.
async fn any(hub: &Hub, id: &str) -> R<Option<Agentenv>> {
    let i = id.to_owned();
    let found = hub.store.read(move |c| Ok(get_in(c, &i)?)).await?;
    Ok(found.map(|r| r.read(Vec::new())).transpose()?)
}

/// A new environment of `person`'s: version 1.
pub async fn create(hub: &Hub, person: &str, draft: Draft) -> R<Agentenv> {
    let name = draft.name.trim().to_owned();
    if let Some(why) = name_problem(&name) {
        return Err(Refusal::BadVar(why));
    }
    checked(hub, person, &draft.vars).await?;
    let vars = serde_json::to_string(&draft.vars).map_err(anyhow::Error::from)?;
    let tags = serde_json::to_string(&tags_of(draft.tags).map_err(Refusal::BadVar)?).map_err(anyhow::Error::from)?;
    let (id, owner, now) = (crate::util::new_id(), person.to_owned(), crate::util::now_unix());
    let (i, n, d, b) = (id.clone(), name.clone(), draft.description.clone(), base_text(draft.base));
    let made = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            if named_in(&tx, &owner, &n, &i)? {
                return Ok(None);
            }
            tx.execute("INSERT INTO agentenvs (id, owner, name, description, base, vars, tags, version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)", params![i, owner, n, d, b, vars, tags, now])?;
            let r = get_in(&tx, &i)?;
            tx.commit()?;
            Ok(r)
        })
        .await?;
    let made = made.ok_or_else(|| taken(&name))?.read(Vec::new())?;
    told(hub, &made.owner, &made.id, false);
    Ok(made)
}

/// Environment `id` of `person`'s changed as `patch` says, over its version `version`: one version up.
pub async fn update(hub: &Hub, person: &str, id: &str, version: i64, patch: Patch) -> R<Agentenv> {
    let old = get(hub, person, id).await?;
    if old.version != version {
        return Err(Refusal::Stale { asked: version, now: old.version });
    }
    let name = patch.name.map_or_else(|| old.name.clone(), |n| n.trim().to_owned());
    if let Some(why) = name_problem(&name) {
        return Err(Refusal::BadVar(why));
    }
    let vars = patch.vars.unwrap_or_else(|| old.vars.clone());
    checked(hub, person, &vars).await?;
    let description = patch.description.unwrap_or_else(|| old.description.clone());
    let base = patch.base.unwrap_or(old.base);
    let tags = match patch.tags {
        Some(t) => tags_of(t).map_err(Refusal::BadVar)?,
        None => old.tags.clone(),
    };
    if name == old.name && description == old.description && base == old.base && vars == old.vars && tags == old.tags {
        return Ok(old);
    }
    let json = serde_json::to_string(&vars).map_err(anyhow::Error::from)?;
    let tags = serde_json::to_string(&tags).map_err(anyhow::Error::from)?;
    let (i, n, now, b) = (id.to_owned(), name.clone(), crate::util::now_unix(), base_text(base));
    let written = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(now_is) = get_in(&tx, &i)? else { return Ok(Written::Missing) };
            // What it was when this change began: another since is its own change, not overwritten.
            if now_is.version != version {
                return Ok(Written::Moved(now_is.version));
            }
            if named_in(&tx, &now_is.owner, &n, &i)? {
                return Ok(Written::Taken);
            }
            tx.execute("UPDATE agentenvs SET name = ?2, description = ?3, base = ?4, vars = ?5, tags = ?6, version = ?7, updated_at = ?8 WHERE id = ?1", params![i, n, description, b, json, tags, version.saturating_add(1), now])?;
            let r = get_in(&tx, &i)?;
            tx.commit()?;
            Ok(r.map_or(Written::Missing, Written::Done))
        })
        .await?;
    match written {
        Written::Done(r) => {
            let e = r.read(sessions_of(hub, id))?;
            told(hub, &e.owner, &e.id, false);
            Ok(e)
        }
        Written::Missing => Err(unknown(id)),
        Written::Taken => Err(taken(&name)),
        Written::Moved(now) => Err(Refusal::Stale { asked: version, now }),
    }
}

/// What a change's write found.
enum Written {
    Done(Record),
    Missing,
    Taken,
    Moved(i64),
}

/// Environment `id` of `person`'s, gone, unless a session runs with it: what it was.
pub async fn delete(hub: &Hub, person: &str, id: &str) -> R<Agentenv> {
    let was = get(hub, person, id).await?;
    if !was.sessions.is_empty() {
        return Err(Refusal::InUse(was.sessions));
    }
    let i = id.to_owned();
    let gone: bool = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let n = tx.execute("DELETE FROM agentenvs WHERE id = ?1", params![i])?;
            // Sessions kiln no longer runs that ran with it: they name nothing now. (One applied to a session meanwhile
            // finds it gone as its harness next starts, and runs with its machine's user's: `resolve::again`.)
            tx.execute("DELETE FROM agentenv_sessions WHERE agentenv_id = ?1", params![i])?;
            tx.commit()?;
            Ok(n > 0)
        })
        .await?;
    if !gone {
        return Err(unknown(id));
    }
    hub.agentenvs.forget(id);
    told(hub, &was.owner, &was.id, true);
    Ok(was)
}

/// The environment session `session` runs with, by id: none, its machine's user's.
pub fn linked(hub: &Hub, session: &str) -> Option<String> {
    hub.agentenvs.links.load().get(session).cloned()
}

/// Session `session` runs with environment `agentenv` from now on (none: its machine's user's), by `by`;
/// both environments' owners are told which sessions run with them now.
pub async fn link(hub: &Hub, session: &str, agentenv: Option<&Agentenv>, by: &str) -> R<()> {
    let was = linked(hub, session);
    let (s, e, now, b) = (session.to_owned(), agentenv.map(|a| a.id.clone()), crate::util::now_unix(), by.to_owned());
    hub.store
        .call(move |c| {
            match &e {
                Some(id) => c.execute(
                    "INSERT INTO agentenv_sessions (session, agentenv_id, applied_at, applied_by) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(session) DO UPDATE SET agentenv_id = ?2, applied_at = ?3, applied_by = ?4",
                    params![s, id, now, b],
                )?,
                None => c.execute("DELETE FROM agentenv_sessions WHERE session = ?1", params![s])?,
            };
            Ok(())
        })
        .await?;
    hub.agentenvs.links.update(|l| {
        match agentenv {
            Some(a) => l.insert(session.to_owned(), a.id.clone()),
            None => l.remove(session),
        };
    });
    if let Some(a) = agentenv {
        told(hub, &a.owner, &a.id, false);
    }
    if let Some(old) = was.filter(|w| agentenv.is_none_or(|a| a.id != *w))
        && let Ok(Some(o)) = any(hub, &old).await
    {
        told(hub, &o.owner, &o.id, false);
    }
    Ok(())
}

/// What the server holds in memory of environments: which each session runs with (as `agentenv_sessions`
/// keeps it), and the runner process each was last given to ([`resolve::opened`]).
#[derive(Default)]
pub struct Held {
    links: crate::rcu::Rcu<HashMap<String, String>>,
    /// By session: the runner process it was last sent its environment in (`machine/instance/pid/build`).
    given: parking_lot::Mutex<HashMap<String, String>>,
}

impl Held {
    /// What `agentenv_sessions` keeps, read as the server starts.
    pub async fn load(&self, store: &crate::store::Store) -> anyhow::Result<()> {
        let rows: Vec<(String, String)> = store
            .read(|c| {
                let mut st = c.prepare("SELECT session, agentenv_id FROM agentenv_sessions")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;
        self.links.set(rows.into_iter().collect());
        Ok(())
    }

    /// Environment `id` is gone: no session names it.
    fn forget(&self, id: &str) {
        self.links.update(|l| l.retain(|_, e| e != id));
    }

    /// Session `session` was given its environment in runner process `process`.
    fn given_in(&self, session: &str, process: String) {
        self.given.lock().insert(session.to_owned(), process);
    }

    /// Whether session `session` was last given its environment in runner process `process`.
    fn was_given_in(&self, session: &str, process: &str) -> bool {
        self.given.lock().get(session).is_some_and(|p| p == process)
    }
}

#[cfg(test)]
mod tests;
