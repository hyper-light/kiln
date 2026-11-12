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
//!
//! kiln.db keeps the records; a text is bytes, kept in kiln's store of bytes (`crate::blobs`,
//! docs/blobs.md) under its content's SHA-256 ([`key`]), once for every Agentfile and version that has
//! it, whoever's. A record names its text by that digest, and is written only once the text is kept; a
//! text is read back whole and held to its digest. A text no record names any more is let go.

pub mod builtin;
pub mod check;
pub mod ops;
pub mod unpack;

use std::collections::HashMap;

use kiln_seams::blobs::{Blobs, Cond, Put, Refusal};
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
    /// kiln's store of bytes couldn't keep a text, or give one back as its digest says: nothing was
    /// kept that names it.
    #[error("{0}")]
    Store(String),
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

/// What kiln.db keeps of an Agentfile: its text named by its SHA-256 ([`key`]).
#[derive(Debug, Clone)]
struct Record {
    id: String,
    owner: String,
    name: String,
    digest: String,
    version: i64,
    created_at: i64,
    updated_at: i64,
}

impl Record {
    fn with(self, text: String) -> Agentfile {
        Agentfile { id: self.id, owner: self.owner, name: self.name, text, version: self.version, created_at: self.created_at, updated_at: self.updated_at }
    }

    fn brief(self) -> Brief {
        Brief { id: self.id, owner: self.owner, name: self.name, version: self.version, created_at: self.created_at, updated_at: self.updated_at }
    }
}

const COLUMNS: &str = "id, owner, name, digest, version, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record { id: r.get(0)?, owner: r.get(1)?, name: r.get(2)?, digest: r.get(3)?, version: r.get(4)?, created_at: r.get(5)?, updated_at: r.get(6)? })
}

fn get_in(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Record>> {
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
fn told(hub: &Hub, a: &Record, deleted: bool) {
    hub.emit(&ServerEvent::Agentfile(AgentfileChange { owner: a.owner.clone(), id: a.id.clone(), name: a.name.clone(), version: a.version, deleted }));
}

/// Where an Agentfile's text is kept in kiln's store of bytes: under its content's SHA-256, once for
/// every Agentfile and version that has it, whoever's (docs/blobs.md, "The keys kiln keeps").
pub fn key(digest: &str) -> String {
    format!("agentfiles/sha256/{digest}")
}

/// Texts are kept (the bytes, then the record that names them) and let go (no record names them, then
/// the bytes) one at a time, so a text a write finds kept already isn't let go before the write's record
/// names it. This server is the one that writes them (docs/architecture.md: the store's group of one
/// voter).
static TEXTS: futures::lock::Mutex<()> = futures::lock::Mutex::new(());

/// `text` kept in `blobs`, once: put only where nothing is (`If-None-Match`), the store holding the bytes
/// to their SHA-256 before it keeps them; one kept already is taken by its etag. Its digest.
async fn keep(blobs: &dyn Blobs, text: &str) -> R<String> {
    let digest = crate::util::sha256_hex(text.as_bytes());
    let k = key(&digest);
    let etag = match blobs.put(&k, text.as_bytes().to_vec(), Put { cond: Cond::IfNoneMatch, sha256: Some(digest.clone()) }).await {
        Ok(m) => m.etag,
        Err(Refusal::PreconditionFailed(_)) => blobs.head(&k).await.map_err(|e| unkept(&e))?.etag,
        Err(e) => return Err(unkept(&e)),
    };
    if etag != digest {
        return Err(Error::Store(format!("kiln's store of bytes holds other bytes at {k}: sha256 {etag}")));
    }
    Ok(digest)
}

fn unkept(e: &Refusal) -> Error {
    Error::Store(format!("kiln's store of bytes couldn't keep the Agentfile's text: {e}"))
}

/// The text kept under `digest`, read whole and held to it: the etag the store says, and the bytes it
/// gives.
async fn read(blobs: &dyn Blobs, digest: &str) -> R<String> {
    let k = key(digest);
    let (meta, bytes) = blobs.get(&k, None).await.map_err(|e| Error::Store(format!("kiln's store of bytes couldn't give back the Agentfile's text: {e}")))?;
    let got = crate::util::sha256_hex(&bytes);
    if meta.etag != digest || got != digest {
        return Err(Error::Store(format!("kiln's store of bytes gave back other bytes than {k}: sha256 {got}")));
    }
    String::from_utf8(bytes).map_err(|_| Error::Store(format!("kiln's store of bytes gave back {k} as bytes that aren't text")))
}

/// Whether a record names text `digest`: an Agentfile, or a version of one.
fn named(c: &rusqlite::Connection, digest: &str) -> rusqlite::Result<bool> {
    c.query_row("SELECT EXISTS(SELECT 1 FROM agentfile_versions WHERE digest = ?1) OR EXISTS(SELECT 1 FROM agentfiles WHERE digest = ?1)", params![digest], |r| r.get(0))
}

/// Of `digests`, the texts no record names any more, let go, with [`TEXTS`] held. One the store
/// couldn't delete, or whose records couldn't be read, stays where it is, warned of: named by no
/// record, it's reached by none.
async fn let_go(hub: &Hub, blobs: &dyn Blobs, digests: Vec<String>) {
    for d in digests {
        let asked = d.clone();
        match hub.store.call(move |c| Ok(named(c, &asked)?)).await {
            Ok(true) => {}
            Ok(false) => match blobs.delete(&key(&d)).await {
                Ok(()) => crate::metrics::blobs().collected.inc(&["agentfiles"]),
                Err(e) => tracing::warn!(target: "kiln.blobs", event = "collect", key = %key(&d), error = %e, "an Agentfile's text no record names couldn't be deleted"),
            },
            Err(e) => tracing::warn!(target: "kiln.blobs", event = "collect", key = %key(&d), error = %format!("{e:#}"), "whether an Agentfile's text is still named couldn't be read: it stays"),
        }
    }
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

/// The record of one the caller reaches; another's is as none.
async fn record(hub: &Hub, who: &Who, id: &str) -> R<Record> {
    let id = id.to_owned();
    let r = hub.store.read(move |c| Ok(get_in(c, &id)?)).await?;
    r.filter(|r| who.reaches(&r.owner)).ok_or(Error::Missing)
}

/// One the caller reaches, its text read back from `blobs`; another's is as none.
pub async fn get(hub: &Hub, blobs: &dyn Blobs, who: &Who, id: &str) -> R<Agentfile> {
    let r = record(hub, who, id).await?;
    let text = read(blobs, &r.digest).await?;
    Ok(r.with(text))
}

/// Its versions, newest first, each text read back once however many versions have it.
pub async fn versions(hub: &Hub, blobs: &dyn Blobs, who: &Who, id: &str) -> R<Vec<Version>> {
    record(hub, who, id).await?;
    let id = id.to_owned();
    let rows: Vec<(i64, String, i64, String)> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare("SELECT version, digest, at, by FROM agentfile_versions WHERE agentfile_id = ?1 ORDER BY version DESC")?;
            let rows = st.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut texts: HashMap<String, String> = HashMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for (version, digest, at, by) in rows {
        let text = match texts.get(&digest) {
            Some(t) => t.clone(),
            None => {
                let t = read(blobs, &digest).await?;
                texts.insert(digest, t.clone());
                t
            }
        };
        out.push(Version { version, text, at, by });
    }
    Ok(out)
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

/// A new Agentfile of the caller's, once Shards passes its text: version 1, its text kept in `blobs`
/// before its record is written.
pub async fn create(hub: &Hub, blobs: &dyn Blobs, who: &Who, name: &str, text: &str, by: &str) -> R<Saved> {
    let name = name.trim().to_owned();
    if let Some(p) = name_problem(&name) {
        return Err(Error::Bad(p));
    }
    let check = checked(hub, text).await?;
    let (id, owner) = (crate::util::new_id(), who.person.clone());
    let (o, n, i) = (owner.clone(), name.clone(), id.clone());
    if hub.store.read(move |c| Ok(named_in(c, &o, &n, &i)?)).await? {
        return Err(taken(&name));
    }
    let held = TEXTS.lock().await;
    let digest = keep(blobs, text).await?;
    let (now, by, n, d) = (crate::util::now_unix(), by.to_owned(), name.clone(), digest.clone());
    let made = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            if named_in(&tx, &owner, &n, &id)? {
                return Ok(None);
            }
            tx.execute("INSERT INTO agentfiles (id, owner, name, digest, version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)", params![id, owner, n, d, now])?;
            tx.execute("INSERT INTO agentfile_versions (agentfile_id, version, digest, at, by) VALUES (?1, 1, ?2, ?3, ?4)", params![id, d, now, by])?;
            let a = get_in(&tx, &id)?;
            tx.commit()?;
            Ok(a)
        })
        .await;
    let made = match made {
        Ok(Some(r)) => r,
        unwritten => {
            // Kept for a record that wasn't written: let go, unless another names it.
            let_go(hub, blobs, vec![digest]).await;
            return Err(unwritten.map_or_else(Error::from, |_| taken(&name)));
        }
    };
    drop(held);
    told(hub, &made, false);
    Ok(Saved { agentfile: made.with(text.to_owned()), check })
}

/// A change: what's given changes. A new text is a new version, once Shards passes it; a new name
/// alone is none.
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub name: Option<String>,
    pub text: Option<String>,
}

/// Agentfile `id` changed as `patch` says, by `by`: a new text kept in `blobs` before the record that
/// names it.
pub async fn update(hub: &Hub, blobs: &dyn Blobs, who: &Who, id: &str, patch: Patch, by: &str) -> R<Saved> {
    let old = record(hub, who, id).await?;
    let name = patch.name.map_or_else(|| old.name.clone(), |n| n.trim().to_owned());
    if let Some(p) = name_problem(&name) {
        return Err(Error::Bad(p));
    }
    let (text, new_version) = match patch.text {
        Some(t) => {
            let new = crate::util::sha256_hex(t.as_bytes()) != old.digest;
            (t, new)
        }
        None => (read(blobs, &old.digest).await?, false),
    };
    let check = if new_version { checked(hub, &text).await? } else { check::check(hub, &text).await };
    if name == old.name && !new_version {
        return Ok(Saved { agentfile: old.with(text), check });
    }
    let held = TEXTS.lock().await;
    let digest = if new_version { keep(blobs, &text).await? } else { old.digest.clone() };
    let (id, now, by, n, d, was) = (id.to_owned(), crate::util::now_unix(), by.to_owned(), name.clone(), digest.clone(), old.version);
    let written = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(now_is) = get_in(&tx, &id)? else { return Ok(Written::Missing) };
            // What it was when this change began: another since is its own change, not overwritten.
            if now_is.version != was {
                return Ok(Written::Moved);
            }
            if named_in(&tx, &now_is.owner, &n, &id)? {
                return Ok(Written::Taken);
            }
            let version = if new_version { was.saturating_add(1) } else { was };
            tx.execute("UPDATE agentfiles SET name = ?2, digest = ?3, version = ?4, updated_at = ?5 WHERE id = ?1", params![id, n, d, version, now])?;
            if new_version {
                tx.execute("INSERT INTO agentfile_versions (agentfile_id, version, digest, at, by) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, version, d, now, by])?;
            }
            let a = get_in(&tx, &id)?;
            tx.commit()?;
            Ok(a.map_or(Written::Missing, Written::Done))
        })
        .await;
    let refused = match written {
        Ok(Written::Done(a)) => {
            drop(held);
            told(hub, &a, false);
            return Ok(Saved { agentfile: a.with(text), check });
        }
        Ok(Written::Missing) => Error::Missing,
        Ok(Written::Taken) => taken(&name),
        Ok(Written::Moved) => Error::Conflict("it changed since you read it: read it again, then change it".into()),
        Err(e) => Error::from(e),
    };
    if new_version {
        // Kept for a version that wasn't written: let go, unless another names it.
        let_go(hub, blobs, vec![digest]).await;
    }
    Err(refused)
}

/// What a change's write found.
enum Written {
    Done(Record),
    Missing,
    Taken,
    /// Another change came first.
    Moved,
}

/// Version `version`'s text made current again, as a new version, so nothing is lost.
pub async fn restore(hub: &Hub, blobs: &dyn Blobs, who: &Who, id: &str, version: i64, by: &str) -> R<Saved> {
    let a = record(hub, who, id).await?;
    let i = id.to_owned();
    let digest: Option<String> = hub.store.read(move |c| Ok(c.query_row("SELECT digest FROM agentfile_versions WHERE agentfile_id = ?1 AND version = ?2", params![i, version], |r| r.get(0)).optional()?)).await?;
    let digest = digest.ok_or_else(|| Error::Bad(format!("the Agentfile has no version {version}")))?;
    let text = read(blobs, &digest).await?;
    if digest == a.digest {
        // As it is already: nothing to make again.
        let check = check::check(hub, &text).await;
        return Ok(Saved { agentfile: a.with(text), check });
    }
    update(hub, blobs, who, id, Patch { name: None, text: Some(text) }, by).await
}

/// Agentfile `id` and its versions, gone: what it was. Their texts no other record names are let go
/// from `blobs`.
pub async fn delete(hub: &Hub, blobs: &dyn Blobs, who: &Who, id: &str) -> R<Brief> {
    let a = record(hub, who, id).await?;
    let i = id.to_owned();
    let gone: Option<Vec<String>> = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let mut st = tx.prepare("SELECT digest FROM agentfile_versions WHERE agentfile_id = ?1 UNION SELECT digest FROM agentfiles WHERE id = ?1")?;
            let digests = st.query_map(params![i], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            drop(st);
            let n = tx.execute("DELETE FROM agentfiles WHERE id = ?1", params![i])?;
            tx.execute("DELETE FROM agentfile_versions WHERE agentfile_id = ?1", params![i])?;
            tx.commit()?;
            Ok((n > 0).then_some(digests))
        })
        .await?;
    let Some(digests) = gone else { return Err(Error::Missing) };
    let held = TEXTS.lock().await;
    let_go(hub, blobs, digests).await;
    drop(held);
    told(hub, &a, true);
    Ok(a.brief())
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
