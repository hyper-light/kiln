//! Agentfiles people keep in kiln (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 30): an Agentfile
//! is Shards' image of an agent's microVM (a Dockerfile's instructions, and `AGENT`, `HARNESS`, `SKILL`,
//! `MCP`, `NETWORK`, `CONNECT`, `ATTACH` and `EXPOSE … FOR`). Each is a person's own (`owner`: the
//! server's `owner`, or `user:<id>`, as an agent session's opener is), named once among theirs, its text
//! checked by Shards before it's kept ([`check`]), and every change of its text a version, a restore
//! too. A person reaches only their own; the server's owner, and whoever has `agentfiles.manage` (an
//! admin), reach everyone's. Listed beside them, read only: those kiln ships ([`builtin`]) and a
//! project's `.kiln/images/<name>/Agentfile` at its default branch. The same operations serve the
//! console (`/api/agentfiles`), agents (kiln's MCP server: `agentfiles_*`) and code mode
//! (`kiln.agentfiles.*`): [`ops`].

pub mod builtin;
pub mod check;
pub mod ops;

use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;

use crate::authz::{Caller, Scope};
use crate::hub::Hub;
use crate::wire::events::{AgentfileChange, ServerEvent};

pub use check::Check;

/// What reaches everyone's Agentfiles (an admin's, as the role has every capability).
pub const MANAGE: &str = "agentfiles.manage";

/// An Agentfile's most bytes: the most buildx reads of a Dockerfile (`maxDockerfileSize`, buildx
/// v0.37.1 build/url.go), which Shards holds the Dockerfiles it fetches to (shards'
/// `crates/shards/src/build/mod.rs`).
pub const MOST: usize = 2 << 20;

/// An Agentfile, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Agentfile {
    pub id: String,
    /// Whose it is: `owner` (the server's), or `user:<id>`.
    pub owner: String,
    /// Its name, an image's (`.kiln/images/<name>/`), once among its owner's.
    pub name: String,
    pub text: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// An Agentfile as a list shows it: without its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileBrief"))]
pub struct Brief {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One version of an Agentfile's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileVersion"))]
pub struct Version {
    pub version: i64,
    pub text: String,
    pub at: i64,
    /// Who made it, as the audit trail names them (`admin`, `user:<name>`, `agent:<id> for …`).
    pub by: String,
}

/// What a change kept, and what Shards said of its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileSaved"))]
pub struct Saved {
    pub agentfile: Agentfile,
    pub check: Check,
}

/// The Agentfiles a list shows: those kept, and beside them, read only, kiln's own and a project's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileListing"))]
pub struct Listing {
    /// The caller's own, or everyone's when it asked for all and reaches them; newest change first.
    pub agentfiles: Vec<Brief>,
    /// Whether the caller reaches everyone's (lists them with `all`): the server's owner, or a person
    /// with `agentfiles.manage`.
    pub reaches_all: bool,
    /// Those kiln ships, one per harness.
    pub builtin: Vec<builtin::View>,
    /// The project asked for (`repo`): its own, at its default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub project: Option<Project>,
}

/// A project's Agentfiles: each `.kiln/images/<name>/Agentfile` at its default branch's head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileProject"))]
pub struct Project {
    /// Its repository, `owner/name`.
    pub repo: String,
    /// The commit read: none while the repository has none.
    pub sha: Option<String>,
    pub agentfiles: Vec<ProjectFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileProjectFile"))]
pub struct ProjectFile {
    /// Its folder's name: the image it builds.
    pub name: String,
    pub path: String,
    /// Its text; none for one past 2 MiB (`MOST`).
    pub text: Option<String>,
}

/// A failure the caller can act on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Bad(String),
    #[error("no such Agentfile")]
    Missing,
    /// Its owner has another of that name, or another change came first.
    #[error("{0}")]
    Conflict(String),
    /// Shards refuses its text: Shards' own words.
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Forbidden(String),
    /// No repository the caller may read is named so.
    #[error("no repository {0}")]
    NoRepo(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

type R<T> = Result<T, Error>;

/// Who asks: whose Agentfiles are theirs, and whether they reach everyone's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    /// `owner`, or `user:<id>` (`crate::harness::lineage::person_of`): an agent's, the person it acts for.
    pub person: String,
    /// The server's owner, or a person with `agentfiles.manage`; never an agent, which reaches only its
    /// person's (as it reaches agent sessions: docs/agents.md, "Whose agents").
    pub all: bool,
}

impl Who {
    pub fn of(caller: &Caller) -> R<Who> {
        let person = crate::harness::lineage::person_of(caller).ok_or_else(|| Error::Forbidden(format!("{} acts for no person here, and an Agentfile is a person's", caller.name)))?;
        Ok(Who { person, all: sees_all(caller) })
    }

    /// Whether `owner`'s Agentfiles are theirs to reach.
    fn reaches(&self, owner: &str) -> bool {
        self.all || self.person == owner
    }
}

/// Whether `caller` reaches everyone's Agentfiles: the server's owner, or a person with `agentfiles.manage`.
pub fn sees_all(caller: &Caller) -> bool {
    caller.agent.is_none() && (caller.is_owner() || caller.can(MANAGE, &Scope::Server))
}

/// Whether `caller` hears of `owner`'s Agentfiles changing (`ServerEvent::Agentfile`): its own, or
/// everyone's where it reaches them.
pub fn hears(caller: &Caller, owner: &str) -> bool {
    Who::of(caller).is_ok_and(|w| w.reaches(owner))
}

/// What's wrong with `name` as an Agentfile's, if anything: it's an image's (`kiln/build-image@v1`'s
/// `.kiln/images/<name>/`, held to the registry's names: lowercase letters and digits, joined by `.`,
/// `_`, `__` or runs of `-`, 255 at the most).
pub fn name_problem(name: &str) -> Option<String> {
    (name.contains('/') || !crate::ci::registry::valid_name(name))
        .then(|| format!("`{name}` isn't an Agentfile's name: an image's, lowercase letters and digits joined by `.`, `_` or `-` (`claude-rust`), 255 at the most"))
}

/// What's wrong with `text` as an Agentfile's before Shards reads it, if anything.
pub fn text_problem(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return Some("an Agentfile needs its instructions".into());
    }
    (text.len() > MOST).then(|| format!("an Agentfile is {} MiB at the most", MOST >> 20))
}

const COLUMNS: &str = "id, owner, name, text, version, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Agentfile> {
    Ok(Agentfile { id: r.get(0)?, owner: r.get(1)?, name: r.get(2)?, text: r.get(3)?, version: r.get(4)?, created_at: r.get(5)?, updated_at: r.get(6)? })
}

fn get_in(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Agentfile>> {
    c.query_row(&format!("SELECT {COLUMNS} FROM agentfiles WHERE id = ?1"), params![id], row).optional()
}

/// Whether `owner` has an Agentfile named `name` other than `id`.
fn named_in(c: &rusqlite::Connection, owner: &str, name: &str, id: &str) -> rusqlite::Result<bool> {
    c.query_row("SELECT EXISTS(SELECT 1 FROM agentfiles WHERE owner = ?1 AND name = ?2 AND id <> ?3)", params![owner, name, id], |r| r.get(0))
}

fn taken(name: &str) -> Error {
    Error::Conflict(format!("you have an Agentfile named {name} already: give this one another name"))
}

/// Tells the events sockets an Agentfile changed or went: its owner's, and those of whoever reaches
/// everyone's (`api`'s `AgentsSeen`).
fn told(hub: &Hub, a: &Agentfile, deleted: bool) {
    hub.emit(&ServerEvent::Agentfile(AgentfileChange { owner: a.owner.clone(), id: a.id.clone(), name: a.name.clone(), version: a.version, deleted }));
}

/// The caller's own, or with `everyone` everyone's, newest change first.
pub async fn list(hub: &Hub, who: &Who, everyone: bool) -> R<Vec<Brief>> {
    if everyone && !who.all {
        return Err(Error::Forbidden(format!("everyone's Agentfiles are for the server's owner and those with {MANAGE}")));
    }
    let owner = (!everyone).then(|| who.person.clone());
    Ok(hub
        .store
        .read(move |c| {
            let mut st = c.prepare("SELECT id, owner, name, version, created_at, updated_at FROM agentfiles WHERE ?1 IS NULL OR owner = ?1 ORDER BY updated_at DESC, id")?;
            let rows = st
                .query_map(params![owner], |r| Ok(Brief { id: r.get(0)?, owner: r.get(1)?, name: r.get(2)?, version: r.get(3)?, created_at: r.get(4)?, updated_at: r.get(5)? }))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?)
}

/// One the caller reaches; another's is as none.
pub async fn get(hub: &Hub, who: &Who, id: &str) -> R<Agentfile> {
    let id = id.to_owned();
    let a = hub.store.read(move |c| Ok(get_in(c, &id)?)).await?;
    a.filter(|a| who.reaches(&a.owner)).ok_or(Error::Missing)
}

/// Its versions, newest first.
pub async fn versions(hub: &Hub, who: &Who, id: &str) -> R<Vec<Version>> {
    get(hub, who, id).await?;
    let id = id.to_owned();
    Ok(hub
        .store
        .read(move |c| {
            let mut st = c.prepare("SELECT version, text, at, by FROM agentfile_versions WHERE agentfile_id = ?1 ORDER BY version DESC")?;
            let rows = st.query_map(params![id], |r| Ok(Version { version: r.get(0)?, text: r.get(1)?, at: r.get(2)?, by: r.get(3)? }))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?)
}

/// `text` as Shards checks it: refused, in Shards' words, when Shards says it's no Agentfile.
async fn checked(hub: &Hub, text: &str) -> R<Check> {
    if let Some(p) = text_problem(text) {
        return Err(Error::Bad(p));
    }
    let c = check::check(hub, text).await;
    match c.refusal() {
        Some(said) => Err(Error::Refused(said)),
        None => Ok(c),
    }
}

/// A new Agentfile of the caller's, once Shards passes its text: version 1.
pub async fn create(hub: &Hub, who: &Who, name: &str, text: &str, by: &str) -> R<Saved> {
    let name = name.trim().to_owned();
    if let Some(p) = name_problem(&name) {
        return Err(Error::Bad(p));
    }
    let check = checked(hub, text).await?;
    let (id, now, owner, text, by) = (crate::util::new_id(), crate::util::now_unix(), who.person.clone(), text.to_owned(), by.to_owned());
    let n = name.clone();
    let made = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            if named_in(&tx, &owner, &n, &id)? {
                return Ok(None);
            }
            tx.execute("INSERT INTO agentfiles (id, owner, name, text, version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)", params![id, owner, n, text, now])?;
            tx.execute("INSERT INTO agentfile_versions (agentfile_id, version, text, at, by) VALUES (?1, 1, ?2, ?3, ?4)", params![id, text, now, by])?;
            let a = get_in(&tx, &id)?;
            tx.commit()?;
            Ok(a)
        })
        .await?;
    let agentfile = made.ok_or_else(|| taken(&name))?;
    told(hub, &agentfile, false);
    Ok(Saved { agentfile, check })
}

/// A change: what's given changes. A new text is a new version, once Shards passes it; a new name
/// alone is none.
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub name: Option<String>,
    pub text: Option<String>,
}

/// Agentfile `id` changed as `patch` says, by `by`.
pub async fn update(hub: &Hub, who: &Who, id: &str, patch: Patch, by: &str) -> R<Saved> {
    let old = get(hub, who, id).await?;
    let name = patch.name.map_or_else(|| old.name.clone(), |n| n.trim().to_owned());
    if let Some(p) = name_problem(&name) {
        return Err(Error::Bad(p));
    }
    let text = patch.text.unwrap_or_else(|| old.text.clone());
    let check = if text == old.text { check::check(hub, &text).await } else { checked(hub, &text).await? };
    if name == old.name && text == old.text {
        return Ok(Saved { agentfile: old, check });
    }
    let new_version = text != old.text;
    let (id, now, by, n) = (id.to_owned(), crate::util::now_unix(), by.to_owned(), name.clone());
    let written = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(now_is) = get_in(&tx, &id)? else { return Ok(Written::Missing) };
            // What it was when this change began: another since is its own change, not overwritten.
            if now_is.version != old.version {
                return Ok(Written::Moved);
            }
            if named_in(&tx, &now_is.owner, &n, &id)? {
                return Ok(Written::Taken);
            }
            let version = if new_version { old.version.saturating_add(1) } else { old.version };
            tx.execute("UPDATE agentfiles SET name = ?2, text = ?3, version = ?4, updated_at = ?5 WHERE id = ?1", params![id, n, text, version, now])?;
            if new_version {
                tx.execute("INSERT INTO agentfile_versions (agentfile_id, version, text, at, by) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, version, text, now, by])?;
            }
            let a = get_in(&tx, &id)?;
            tx.commit()?;
            Ok(a.map_or(Written::Missing, Written::Done))
        })
        .await?;
    let agentfile = match written {
        Written::Done(a) => a,
        Written::Missing => return Err(Error::Missing),
        Written::Taken => return Err(taken(&name)),
        Written::Moved => return Err(Error::Conflict("it changed since you read it: read it again, then change it".into())),
    };
    told(hub, &agentfile, false);
    Ok(Saved { agentfile, check })
}

/// What a change's write found.
enum Written {
    Done(Agentfile),
    Missing,
    Taken,
    /// Another change came first.
    Moved,
}

/// Version `version`'s text made current again, as a new version, so nothing is lost.
pub async fn restore(hub: &Hub, who: &Who, id: &str, version: i64, by: &str) -> R<Saved> {
    let old = versions(hub, who, id).await?.into_iter().find(|v| v.version == version).ok_or_else(|| Error::Bad(format!("the Agentfile has no version {version}")))?;
    let a = get(hub, who, id).await?;
    if a.text == old.text {
        // As it is already: nothing to make again.
        let check = check::check(hub, &a.text).await;
        return Ok(Saved { agentfile: a, check });
    }
    update(hub, who, id, Patch { name: None, text: Some(old.text) }, by).await
}

/// Agentfile `id` and its versions, gone: what it was.
pub async fn delete(hub: &Hub, who: &Who, id: &str) -> R<Agentfile> {
    let a = get(hub, who, id).await?;
    let i = id.to_owned();
    let gone = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let n = tx.execute("DELETE FROM agentfiles WHERE id = ?1", params![i])?;
            tx.execute("DELETE FROM agentfile_versions WHERE agentfile_id = ?1", params![i])?;
            tx.commit()?;
            Ok(n > 0)
        })
        .await?;
    if !gone {
        return Err(Error::Missing);
    }
    told(hub, &a, true);
    Ok(a)
}

/// The Agentfiles a list shows, for `caller`: the kept ones it reaches ([`list`]), kiln's own, and with
/// `repo`, that project's.
pub async fn listing(hub: &Hub, caller: &Caller, everyone: bool, repo: Option<&str>) -> R<Listing> {
    let who = Who::of(caller)?;
    let agentfiles = list(hub, &who, everyone).await?;
    let project = match repo.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => Some(project(hub, caller, r).await?),
        None => None,
    };
    Ok(Listing { agentfiles, reaches_all: who.all, builtin: builtin::views(), project })
}

/// Project `repo`'s Agentfiles (by `owner/name` or id), for a caller that reads its runs; one it may
/// not is as none.
pub async fn project(hub: &Hub, caller: &Caller, repo: &str) -> R<Project> {
    let repos = hub.store.repos().await?;
    let Some(r) = repos.into_iter().find(|r| r.id == repo || r.name.eq_ignore_ascii_case(repo)).filter(|r| caller.can("runs.read", &Scope::Repo { repo: r.name.clone() })) else {
        return Err(Error::NoRepo(repo.to_owned()));
    };
    let Some(sha) = crate::ci::load::resolve(hub, &r, "HEAD").await? else {
        return Ok(Project { repo: r.name, sha: None, agentfiles: Vec::new() });
    };
    let dir = crate::gitops::ensure_commit(hub, &r, &sha).await?;
    let d = dir.to_string_lossy().into_owned();
    let listed = crate::gitops::run_git(hub, &["-C", &d, "ls-tree", "-r", "-l", "-z", &sha, "--", ".kiln/images"], crate::gitops::LS_REMOTE_TIMEOUT).await?;
    let mut agentfiles = Vec::new();
    for (oid, size, path) in String::from_utf8_lossy(&listed).split('\0').filter_map(tree_entry) {
        let Some(name) = path.strip_prefix(".kiln/images/").and_then(|p| p.strip_suffix("/Agentfile")).filter(|n| name_problem(n).is_none()) else { continue };
        let text = match size {
            Some(s) if s <= MOST => Some(String::from_utf8_lossy(&crate::gitops::run_git(hub, &["-C", &d, "cat-file", "blob", oid], crate::gitops::LS_REMOTE_TIMEOUT).await?).into_owned()),
            _ => None,
        };
        agentfiles.push(ProjectFile { name: name.to_owned(), path: path.to_owned(), text });
    }
    Ok(Project { repo: r.name, sha: Some(sha), agentfiles })
}

/// One entry of `git ls-tree -l -z`: a blob's object, size and path (`<mode> blob <oid> <size>\t<path>`).
fn tree_entry(line: &str) -> Option<(&str, Option<usize>, &str)> {
    let (meta, path) = line.split_once('\t')?;
    let mut f = meta.split_whitespace();
    let (_, kind, oid, size) = (f.next()?, f.next()?, f.next()?, f.next()?);
    (kind == "blob").then(|| (oid, size.parse().ok(), path))
}

#[cfg(test)]
mod tests;
