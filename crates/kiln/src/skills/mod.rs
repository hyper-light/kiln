//! Skills kiln installs for agents (docs/agents.md, "Skills kiln installs"): from a link (a GitHub
//! repository, a folder or file in one, any git URL), a folder on an agent's machine, an upload,
//! or words written in the page; read as one package ([`detect`]), pinned to its commit, its files
//! kept once each in kiln's store of bytes by their SHA-256. A package is for one agent, for every
//! agent of the person who installed it, or for every agent working in one repository's folder on
//! one machine ([`Scope`]). Each machine with such agents lays it out and sets it up ([`place`]);
//! each session there loads its skills through its harness or the shared prompt bridge.
//!
//! Looking at a source makes a draft: what it holds, to install or not. Installing one picks its
//! parts (skills, agents; hooks and MCP servers only when turned on: they run code on their own)
//! and whom it's for. Nothing an agent asks installs anything: installs are a person's.

pub use kiln_ci::skills as detect;
pub mod bridge;
pub mod ops;
pub mod place;
pub mod runner;
pub mod source;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::hub::Hub;
use detect::{File, Item, Package, Picks};
use place::{FileRef, Manifest, Pkg, State};
use source::Source;

/// The name a runner and the server say they speak this by (docs/wire.md, "Named features").
pub const FEATURE: &str = crate::proto::feature::SKILLS;
/// What a file's piece on the wire holds at most (as an attachment's, `harness::registry`).
pub const PIECE: usize = 512 * 1024;

/// Whom a package is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// One agent.
    Agent { agent: String },
    /// Every agent of whoever installed it (`owner`, `user:<id>`: whom an agent acts for).
    Mine { opener: String },
    /// Every agent working in one repository's folder on one machine.
    Repo { machine: String, root: String },
}

impl Scope {
    /// Narrowest first: what one agent was given goes before what all of a person's were.
    fn rank(&self) -> u8 {
        match self {
            Self::Agent { .. } => 0,
            Self::Repo { .. } => 1,
            Self::Mine { .. } => 2,
        }
    }

    pub fn word(&self) -> &'static str {
        match self {
            Self::Agent { .. } => "This agent",
            Self::Repo { .. } => "This repo",
            Self::Mine { .. } => "All my agents",
        }
    }
}

/// A session as scopes see it: where it runs and works, and whom it acts for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct At {
    pub id: String,
    pub machine: String,
    /// Its working tree (else its folder).
    pub root: String,
    pub opener: Option<String>,
}

impl Scope {
    pub fn applies(&self, s: &At) -> bool {
        match self {
            Self::Agent { agent } => *agent == s.id,
            Self::Mine { opener } => s.opener.as_deref() == Some(opener.as_str()),
            Self::Repo { machine, root } => !root.is_empty() && *machine == s.machine && (s.root == *root || s.root.strip_prefix(root.as_str()).is_some_and(|r| r.starts_with(['/', '\\']))),
        }
    }
}

/// A package installed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    pub id: String,
    pub source: Source,
    /// Its commit (a link's), else its content's digest, short.
    pub sha: String,
    pub package: Package,
    pub picks: Picks,
    /// Its files as installed, its manifest among them.
    pub files: Vec<FileRef>,
    pub scope: Scope,
    /// Who installed it.
    pub by: String,
    pub on: bool,
    pub at: i64,
    pub updated: i64,
    /// A newer commit its link's branch is at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
    /// When its link was last looked at for one (ms).
    #[serde(default)]
    pub checked: i64,
}

impl Installed {
    fn pkg(&self) -> Pkg {
        Pkg { id: self.id.clone(), name: self.package.name.clone(), sha: self.sha.clone(), installed_at: self.updated, files: self.files.clone(), setup: if self.picks.setup && self.picks.skills { self.package.setup.clone() } else { Vec::new() }, needs: if self.picks.skills { self.package.needs.clone() } else { Vec::new() } }
    }
}

/// What looking at a source found, kept to install.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub id: String,
    pub by: String,
    pub source: Source,
    pub sha: String,
    pub package: Package,
    /// Every file of the package as read (all parts), by its installed path.
    pub files: Vec<FileRef>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Bad(String),
    #[error("{0}")]
    Missing(String),
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type R<T> = Result<T, Error>;

// W20-R01: keeping bytes and publishing/removing their durable references are one mutation.
// Otherwise a concurrent removal can collect bytes another draft has kept but not recorded yet.
static MUTATIONS: crate::turn::Gate = crate::turn::Gate::one();

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn blob_key(digest: &str) -> String {
    format!("skills/files/{digest}")
}

/// Keeps each file in kiln's store of bytes by its digest, once: its reference.
async fn keep(hub: &Hub, files: Vec<File>) -> R<Vec<FileRef>> {
    // W20-R01: hashing an upload's assets must not stall an HTTP/runtime shard.
    let hashed: Vec<(File, String)> = crate::rt::blocking(move || files.into_iter().map(|f| {
        let digest = hex(&Sha256::digest(&f.bytes));
        (f, digest)
    }).collect()).await?;
    let mut out = Vec::with_capacity(hashed.len());
    for (f, digest) in hashed {
        let key = blob_key(&digest);
        let size = f.bytes.len() as u64;
        if hub.blobs.blobs.head(&key).await.is_err() {
            hub.blobs.blobs.put(&key, f.bytes, kiln_seams::blobs::Put::default()).await.map_err(|e| Error::Other(anyhow::anyhow!("keeping {}: {e}", f.path)))?;
        }
        out.push(FileRef { path: f.path, sha256: digest, size, exec: f.exec });
    }
    Ok(out)
}

/// A draft from a source's files: read as a package, kept, and stored for its installer. A
/// marketplace with plugins to choose among answers them instead.
pub async fn look(hub: &Hub, by: &str, src: Source, sha: String, files: Vec<File>, want: Option<&str>, fallback: &str) -> R<Result<Draft, Vec<Item>>> {
    let _mutation = MUTATIONS.turn().await;
    let (want, fallback) = (want.map(str::to_owned), fallback.to_owned());
    let read = crate::rt::blocking(move || detect::read(&files, want.as_deref(), &fallback)).await?;
    let mut pkg = match read {
        Ok(detect::Read::Package(p)) => *p,
        Ok(detect::Read::Choose(items)) => return Ok(Err(items)),
        Err(e) => return Err(Error::Bad(e.to_string())),
    };
    let old: BTreeSet<String> = {
        let by = by.to_owned();
        let row: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT body FROM skill_drafts WHERE by = ?1 LIMIT 1", params![by], |r| r.get(0)).optional()?)).await?;
        row.and_then(|body| serde_json::from_str::<Draft>(&body).ok()).map(|d| d.files.into_iter().map(|f| f.sha256).collect()).unwrap_or_default()
    };
    let refs = keep(hub, std::mem::take(&mut pkg.files)).await?;
    // A source without a commit (an upload, a folder, words) is named by its content.
    let sha = if sha.is_empty() { hex(&Sha256::digest(refs.iter().map(|f| format!("{} {}\n", f.path, f.sha256)).collect::<String>().as_bytes())).chars().take(12).collect() } else { sha };
    let d = Draft { id: crate::util::new_id(), by: by.to_owned(), source: src, sha, package: pkg, files: refs };
    let (row, by) = (serde_json::to_string(&d).map_err(anyhow::Error::from)?, by.to_owned());
    let id = d.id.clone();
    // One draft a person: a new look replaces the last.
    hub.store
        .call(move |c| {
            c.execute("DELETE FROM skill_drafts WHERE by = ?1", params![by])?;
            c.execute("INSERT INTO skill_drafts (id, by, body, created_at) VALUES (?1, ?2, ?3, ?4)", params![id, by, row, crate::util::now_ms()])?;
            Ok(())
        })
        .await?;
    forget(hub, old).await;
    Ok(Ok(d))
}

async fn draft(hub: &Hub, id: &str) -> R<Draft> {
    let id = id.to_owned();
    let row: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT body FROM skill_drafts WHERE id = ?1", params![id], |r| r.get(0)).optional()?)).await?;
    let row = row.ok_or_else(|| Error::Missing("no such draft: look at its source again".into()))?;
    serde_json::from_str(&row).map_err(|e| Error::Other(e.into()))
}

/// Every package installed, oldest first.
pub async fn all(hub: &Hub) -> R<Vec<Installed>> {
    read(&rows(hub).await?)
}

/// Every package's row as it's kept (its JSON), oldest first: what [`all`] reads, and [`sync`] off the shards.
async fn rows(hub: &Hub) -> R<Vec<String>> {
    Ok(hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT body FROM skill_packages ORDER BY created_at, id")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?)
}

/// Packages' rows ([`rows`]), read.
fn read(rows: &[String]) -> R<Vec<Installed>> {
    rows.iter().map(|r| serde_json::from_str(r).map_err(|e| Error::Other(e.into()))).collect()
}

pub async fn get(hub: &Hub, id: &str) -> R<Installed> {
    let id = id.to_owned();
    let row: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT body FROM skill_packages WHERE id = ?1", params![id], |r| r.get(0)).optional()?)).await?;
    let row = row.ok_or_else(|| Error::Missing("no such package".into()))?;
    serde_json::from_str(&row).map_err(|e| Error::Other(e.into()))
}

async fn save(hub: &Hub, p: &Installed) -> R<()> {
    let (id, name, body, at, updated) = (p.id.clone(), p.package.name.clone(), serde_json::to_string(p).map_err(anyhow::Error::from)?, p.at, p.updated);
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO skill_packages (id, name, body, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id) DO UPDATE SET name = excluded.name, body = excluded.body, updated_at = excluded.updated_at",
                params![id, name, body, at, updated],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// The files of a draft that install with `picks`, and its manifest, kept.
async fn chosen(hub: &Hub, pkg: &Package, files: &[FileRef], picks: Picks) -> R<Vec<FileRef>> {
    let mut out: Vec<FileRef> = files.iter().filter(|f| picks.takes(&f.path)).cloned().collect();
    if !out.iter().any(|f| f.path.starts_with("skills/") || f.path.starts_with("agents/")) {
        return Err(Error::Bad("pick its skills or its agents: nothing else installs alone".into()));
    }
    out.extend(keep(hub, vec![File { path: ".claude-plugin/plugin.json".into(), bytes: detect::plugin_json(pkg), exec: false }]).await?);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Installs draft `draft_id` with `picks`, for `scope`, by `by`: in place of the same package
/// installed for the same scope.
pub async fn install(hub: &Hub, by: &str, draft_id: &str, picks: Picks, scope: Scope) -> R<Installed> {
    let _mutation = MUTATIONS.turn().await;
    let d = draft(hub, draft_id).await?;
    if d.by != by {
        return Err(Error::Forbidden("this draft belongs to someone else: look at its source yourself".into()));
    }
    if matches!(&scope, Scope::Repo { root, .. } if root.is_empty() || root == "/") {
        return Err(Error::Bad("the agent has no repository folder to install for".into()));
    }
    let files = chosen(hub, &d.package, &d.files, picks).await?;
    let now = crate::util::now_ms();
    let had = all(hub).await?.into_iter().find(|p| p.package.name == d.package.name && p.scope == scope);
    if had.as_ref().is_some_and(|p| p.by != by) {
        return Err(Error::Forbidden("this package was installed by someone else: they change it".into()));
    }
    let old: BTreeSet<String> = d.files.iter().chain(had.iter().flat_map(|p| p.files.iter())).map(|f| f.sha256.clone()).collect();
    let p = Installed {
        id: had.as_ref().map_or_else(crate::util::new_id, |h| h.id.clone()),
        source: d.source,
        sha: d.sha,
        package: d.package,
        picks,
        files,
        scope,
        by: by.to_owned(),
        on: true,
        at: had.as_ref().map_or(now, |h| h.at),
        updated: now,
        update: None,
        checked: now,
    };
    save(hub, &p).await?;
    let id = d.id;
    hub.store.call(move |c| Ok(c.execute("DELETE FROM skill_drafts WHERE id = ?1", params![id])?)).await?;
    sync(hub).await;
    forget(hub, old).await;
    Ok(p)
}

/// Turns a package on or off; on when already on explicitly retries its setup.
pub async fn set(hub: &Hub, id: &str, on: bool) -> R<Installed> {
    let _mutation = MUTATIONS.turn().await;
    let mut p = get(hub, id).await?;
    p.on = on;
    p.updated = crate::util::now_ms().max(p.updated.saturating_add(1));
    save(hub, &p).await?;
    sync(hub).await;
    Ok(p)
}

/// Uninstalls a package; its files go from kiln's store once nothing else holds them.
pub async fn remove(hub: &Hub, id: &str) -> R<()> {
    let _mutation = MUTATIONS.turn().await;
    let p = get(hub, id).await?;
    let pid = p.id.clone();
    hub.store.call(move |c| Ok(c.execute("DELETE FROM skill_packages WHERE id = ?1", params![pid])?)).await?;
    sync(hub).await;
    forget(hub, p.files.iter().map(|f| f.sha256.clone()).collect()).await;
    Ok(())
}

/// Lets go of files nothing installed or drafted holds.
async fn forget(hub: &Hub, digests: BTreeSet<String>) {
    let held: BTreeSet<String> = match hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT body FROM skill_packages UNION ALL SELECT body FROM skill_drafts")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    {
        Ok(rows) => rows.iter().filter_map(|r| serde_json::from_str::<Value>(r).ok()).flat_map(|v| v.get("files").and_then(Value::as_array).cloned().unwrap_or_default()).filter_map(|f| f.get("sha256").and_then(Value::as_str).map(str::to_owned)).collect(),
        Err(_) => return,
    };
    for d in digests.difference(&held) {
        let _ = hub.blobs.blobs.delete(&blob_key(d)).await;
    }
}

/// Fetches a link's package again at its branch's commit now, keeping what was picked and whom
/// it's for.
pub async fn update(hub: &Hub, id: &str) -> R<Installed> {
    let _mutation = MUTATIONS.turn().await;
    let mut p = get(hub, id).await?;
    if p.source.kind != "link" {
        return Err(Error::Bad("only a link's package updates: look at its source again".into()));
    }
    let l = source::Link { url: p.source.url.clone(), git_ref: p.source.git_ref.clone(), path: p.source.path.clone(), label: p.source.label.clone() };
    let (files, sha) = source::fetch(hub, &l).await?;
    let want = (!p.source.plugin.is_empty()).then(|| p.source.plugin.clone())
        .or_else(|| (!p.source.path.is_empty()).then(|| p.source.path.clone()))
        .or_else(|| (p.package.shape == detect::Shape::Marketplace).then(|| p.package.name.clone()));
    let fallback = p.package.name.clone();
    let read = crate::rt::blocking(move || detect::read(&files, want.as_deref(), &fallback)).await?;
    let mut pkg = match read {
        Ok(detect::Read::Package(pkg)) => *pkg,
        Ok(detect::Read::Choose(_)) => return Err(Error::Bad("its marketplace no longer names it".into())),
        Err(e) => return Err(Error::Bad(e.to_string())),
    };
    let refs = keep(hub, std::mem::take(&mut pkg.files)).await?;
    let old: BTreeSet<String> = p.files.iter().chain(refs.iter()).map(|f| f.sha256.clone()).collect();
    p.files = chosen(hub, &pkg, &refs, p.picks).await?;
    p.package = pkg;
    p.sha = sha;
    p.update = None;
    p.checked = crate::util::now_ms();
    p.updated = p.checked;
    save(hub, &p).await?;
    sync(hub).await;
    forget(hub, old).await;
    Ok(p)
}

/// Looks for newer commits of the links of `pkgs` not looked at for the server's poll interval
/// (`[gitops] poll_seconds`), in the background.
pub fn check(hub: &Arc<Hub>, pkgs: &[Installed]) {
    let every = i64::from(hub.cfg.gitops.poll_seconds.max(1)).saturating_mul(1000);
    let now = crate::util::now_ms();
    let due: Vec<Installed> = pkgs.iter().filter(|p| p.source.kind == "link" && now.saturating_sub(p.checked) >= every).cloned().collect();
    if due.is_empty() {
        return;
    }
    let hub = Arc::clone(hub);
    crate::rt::spawn(async move {
        for mut p in due {
            p.checked = crate::util::now_ms();
            match source::head(&hub, &p.source.url, &p.source.git_ref).await {
                Ok(Some(sha)) if !sha.starts_with(&p.sha) && !p.sha.starts_with(&sha) => p.update = Some(sha),
                Ok(_) => p.update = None,
                Err(e) => tracing::debug!(package = %p.package.name, error = %format!("{e:#}"), "couldn't look for a newer commit"),
            }
            let _ = hub.store.call(move |c| {
                let row: Option<String> = c.query_row("SELECT body FROM skill_packages WHERE id = ?1", params![p.id], |r| r.get(0)).optional()?;
                if let Some(row) = row {
                    let mut current: Installed = serde_json::from_str(&row)?;
                    if current.sha == p.sha && current.updated == p.updated {
                        current.checked = p.checked;
                        current.update = p.update;
                        c.execute("UPDATE skill_packages SET body = ?1 WHERE id = ?2", params![serde_json::to_string(&current)?, current.id])?;
                    }
                }
                Ok(())
            }).await;
        }
    });
}

/// Which installed packages each session has, narrowest scope first; what's off has none.
pub fn for_session(pkgs: &[Installed], s: &At) -> Vec<String> {
    let mut v: Vec<&Installed> = pkgs.iter().filter(|p| p.on && p.scope.applies(s)).collect();
    v.sort_by_key(|p| (p.scope.rank(), p.at));
    v.into_iter().map(|p| p.id.clone()).collect()
}

/// Each machine's manifest: the packages it lays out (one agent's on its machine, one
/// repository's on its own, a person's on every machine `machines` names), and which of its
/// sessions has which.
pub fn manifests(pkgs: &[Installed], sessions: &[At], machines: &BTreeSet<String>) -> BTreeMap<String, Manifest> {
    let mut out: BTreeMap<String, Manifest> = machines.iter().map(|m| (m.clone(), Manifest::default())).collect();
    let on: Vec<&Installed> = pkgs.iter().filter(|p| p.on).collect();
    for p in &on {
        let at: Vec<String> = match &p.scope {
            Scope::Mine { .. } => machines.iter().cloned().collect(),
            Scope::Agent { agent } => sessions.iter().filter(|s| s.id == *agent).map(|s| s.machine.clone()).collect(),
            Scope::Repo { machine, .. } => vec![machine.clone()],
        };
        for m in at {
            if let Some(man) = out.get_mut(&m) {
                man.packages.push(p.pkg());
            }
        }
    }
    for s in sessions {
        let Some(man) = out.get_mut(&s.machine) else { continue };
        let ids = for_session(pkgs, s);
        if !ids.is_empty() {
            man.sessions.insert(s.id.clone(), ids);
        }
    }
    out
}

/// Who opened each session, by its id, as kiln's store keeps it.
async fn openers(hub: &Hub) -> BTreeMap<String, String> {
    hub.store.kv_prefix("agents/opener/".into()).await.unwrap_or_default().into_iter().filter_map(|(k, v)| k.strip_prefix("agents/opener/").map(|id| (id.to_owned(), v))).collect()
}

/// Every session kiln knows (`infos`, as their owners share them), as scopes see them, with who opened each.
fn sessions_at(infos: &[Arc<crate::harness::session::Info>], openers: &BTreeMap<String, String>) -> Vec<At> {
    infos
        .iter()
        .map(|i| At { machine: crate::harness::wait::machine(&i.target).to_owned(), root: i.tree.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| i.cwd.clone()), opener: openers.get(&i.id).cloned(), id: i.id.clone() })
        .collect()
}

/// The machines that lay packages out: the runners speaking [`FEATURE`], and the server's own.
fn machines(hub: &Hub) -> (BTreeSet<String>, BTreeMap<String, crate::instances::PeerId>) {
    let inst = hub.instances.load();
    let mut peers: BTreeMap<String, crate::instances::PeerId> = BTreeMap::new();
    for peer in inst.speaking(FEATURE) {
        if let Some(i) = inst.get(peer) {
            // The newest process of a machine speaks for it.
            let newer = peers.get(&i.name).is_some_and(|p| *p > peer);
            if !newer {
                peers.insert(i.name.clone(), peer);
            }
        }
    }
    let mut names: BTreeSet<String> = peers.keys().cloned().collect();
    names.insert("local".into());
    (names, peers)
}

/// Tells every machine what it lays out now (after an install, an update, a session starting or a
/// runner speaking [`FEATURE`] connecting); the server's own sessions' at once.
pub async fn sync(hub: &Hub) {
    // W20-R01: an older install/session snapshot must never overtake a newer one.
    static SYNC: crate::turn::Gate = crate::turn::Gate::one();
    let _turn = SYNC.turn().await;
    let rows = match rows(hub).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the skills installed");
            return;
        }
    };
    let (openers, infos) = (openers(hub).await, hub.harness.list_shared());
    let (names, peers) = machines(hub);
    // Read, weighed and written out on the compute pool: every package's JSON read, each machine's manifest made from them
    // and every session, and the runners' written out (goal 9, plan 01a1a87b2cec step 3: on the shard that heard of the
    // session starting, a sync held it 0.58 to 0.63 ms on the CPU in probes 18 and 20's Linux rounds, the lab's fill
    // starting 32 sessions).
    let made = crate::rt::compute(move || -> R<Vec<(String, Manifest, Option<String>)>> {
        let pkgs = read(&rows)?;
        let made = manifests(&pkgs, &sessions_at(&infos, &openers), &names);
        Ok(made.into_iter().map(|(machine, m)| {
            let json = (machine != "local").then(|| serde_json::to_string(&json!({"manifest": &m})).unwrap_or_default());
            (machine, m, json)
        }).collect())
    })
    .await;
    let made = match made.map_err(Error::Other).and_then(|m| m) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the skills installed");
            return;
        }
    };
    for (machine, m, json) in made {
        if machine == "local" {
            place::remember("local", &m);
            let k: Arc<dyn place::Keeper> = Arc::new(Local { host: Arc::clone(&hub.harness.local), store: hub.store.clone() });
            let local = place::Place::new(place::root(), "local");
            // The server's own machine reads its files from its store, those it hasn't kept yet.
            let want = { let m = m.clone(); crate::rt::blocking(move || local.want(&m)).await.unwrap_or_default() };
            for d in want {
                if let Ok((_, bytes)) = hub.blobs.blobs.get(&blob_key(&d), None).await {
                    let _ = crate::rt::blocking(move || place::Place::new(place::root(), "local").keep(&d, &bytes)).await;
                }
            }
            crate::rt::spawn(place::settle(k));
            continue;
        }
        if let (Some(peer), Some(json)) = (peers.get(&machine), json) {
            hub.send_peer(*peer, &crate::proto::ServerMsg::Skills { json }).await;
        }
    }
}

/// The server's own sessions' keeper.
struct Local {
    host: Arc<crate::harness::host::Host>,
    store: crate::store::Store,
}

impl place::Keeper for Local {
    fn owner(&self) -> &'static str {
        "local"
    }

    fn live(&self) -> BTreeSet<String> {
        self.host.list().into_iter().map(|i| i.id).collect()
    }

    fn reload(&self, sessions: &[String]) {
        for s in sessions {
            reload(&self.host, s);
        }
    }

    fn report(&self, states: Vec<State>) -> futures::future::BoxFuture<'static, ()> {
        let store = self.store.clone();
        Box::pin(async move {
            let _ = store.kv_set(state_key("local"), serde_json::to_string(&states).unwrap_or_default()).await;
        })
    }
}

/// Refresh native inventories where supported, and the portable bridge's catalogue, which it keeps
/// until told so (`Core::skills_changed`).
pub fn reload(host: &crate::harness::host::Host, session: &str) {
    use crate::harness::model::Command;
    if let Some(s) = host.get(session) {
        if s.info().kind == "claude" {
            host.command(session, Command::Control { id: "reload_plugins".into(), value: None });
        }
        host.command(session, Command::SkillsReload);
    }
}

fn state_key(machine: &str) -> String {
    format!("skills/state/{machine}")
}

async fn heard_states(hub: &Hub, machine: &str, states: Vec<State>) {
    let _ = hub.store.kv_set(state_key(machine), serde_json::to_string(&states).unwrap_or_default()).await;
}

/// Folder reads asked of runners, by their request: what's said of their files, and their bytes.
#[derive(Default)]
struct Reading {
    peer: crate::instances::PeerId,
    files: Option<Vec<FileRef>>,
    error: Option<String>,
    got: BTreeMap<String, Vec<u8>>,
    part: BTreeMap<String, Vec<u8>>,
    done: Option<async_channel::Sender<()>>,
}

static READS: parking_lot::Mutex<BTreeMap<u64, Reading>> = parking_lot::Mutex::new(BTreeMap::new());
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn read_complete(r: &Reading) -> bool {
    r.error.is_some() || r.files.as_ref().is_some_and(|f| f.iter().all(|f| r.got.contains_key(&f.sha256)))
}

/// A folder on `machine`, read: the server's own, from its disk; a runner's, asked of it.
pub async fn read_folder(hub: &Hub, machine: &str, path: &str) -> R<Vec<File>> {
    if machine == "local" {
        let p = expand(path);
        return crate::rt::blocking(move || source::walk(&p)).await.map_err(Error::Other)?.map_err(|e| Error::Bad(format!("{path}: {e}")));
    }
    let (_, peers) = machines(hub);
    let Some(peer) = peers.get(machine).copied() else { return Err(Error::Bad(format!("{machine}'s kiln doesn't install skills yet: upgrade kiln on {machine}"))) };
    let req = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = async_channel::bounded(1);
    READS.lock().insert(req, Reading { peer, done: Some(tx), ..Reading::default() });
    hub.send_peer(peer, &crate::proto::ServerMsg::Skills { json: json!({"read": {"req": req, "path": path}}).to_string() }).await;
    let waited = crate::rt::timeout(crate::gitops::GIT_TIMEOUT, rx.recv()).await;
    let r = READS.lock().remove(&req).unwrap_or_default();
    if waited.is_err() {
        return Err(Error::Bad(format!("{machine} didn't send {path}")));
    }
    if let Some(e) = r.error {
        return Err(Error::Bad(format!("{path} on {machine}: {e}")));
    }
    Ok(r.files.unwrap_or_default().into_iter().filter_map(|f| r.got.get(&f.sha256).map(|b| File { path: f.path, bytes: b.clone(), exec: f.exec })).collect())
}

/// `~` as the home folder of the machine it's read on.
pub fn expand(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => crate::os::home_dir().join(rest.trim_start_matches('/')),
        _ => std::path::PathBuf::from(path),
    }
}

/// [`crate::proto::AgentMsg::Skills`] from `machine`'s runner `peer`: files it wants, its packages'
/// states, or a folder's files it was asked for.
pub async fn heard(hub: &Hub, machine: &str, peer: crate::instances::PeerId, json: &str) {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return };
    if let Some(want) = v.get("want").and_then(Value::as_array) {
        for d in want.iter().filter_map(Value::as_str) {
            let Ok((_, bytes)) = hub.blobs.blobs.get(&blob_key(d), None).await else {
                tracing::warn!(%machine, file = %d, "a runner wants a skill's file kiln doesn't keep");
                continue;
            };
            let mut offset = 0usize;
            loop {
                let end = (offset + PIECE).min(bytes.len());
                let data = bytes.get(offset..end).unwrap_or_default().to_vec();
                hub.send_peer(peer, &crate::proto::ServerMsg::SkillFile { sha256: d.to_owned(), offset: offset as u64, data, done: end >= bytes.len() }).await;
                if end >= bytes.len() {
                    break;
                }
                offset = end;
            }
        }
    }
    if let Some(states) = v.get("states").and_then(|s| serde_json::from_value::<Vec<State>>(s.clone()).ok()) {
        heard_states(hub, machine, states).await;
    }
    if let Some(req) = v.get("read").and_then(Value::as_u64) {
        let mut reads = READS.lock();
        if let Some(r) = reads.get_mut(&req).filter(|r| r.peer == peer) {
            r.error = v.get("error").and_then(Value::as_str).map(str::to_owned);
            r.files = v.get("files").and_then(|f| serde_json::from_value(f.clone()).ok());
            if r.files.as_ref().is_some_and(|files| files.len() > source::MAX_FILES || files.iter().any(|f| f.size > detect::MAX_FILE) || files.iter().map(|f| f.size).fold(0u64, u64::saturating_add) > detect::MAX_TOTAL.saturating_mul(4)) {
                r.files = None;
                r.error = Some("the folder is over its file or byte limit".into());
            }
            if read_complete(r) && let Some(tx) = r.done.take() {
                let _ = tx.try_send(());
            }
        }
    }
}

/// [`crate::proto::AgentMsg::SkillFile`]: a piece of a file of a folder a runner was asked for.
pub fn heard_file(peer: crate::instances::PeerId, req: u64, digest: &str, offset: u64, data: &[u8], done: bool) {
    let mut reads = READS.lock();
    let Some(r) = reads.get_mut(&req).filter(|r| r.peer == peer) else { return };
    let wanted = r.files.as_ref().is_some_and(|files| files.iter().any(|f| f.sha256 == digest));
    if !wanted || r.got.contains_key(digest) {
        return;
    }
    let part = r.part.entry(digest.to_owned()).or_default();
    if usize::try_from(offset).ok() != Some(part.len()) || part.len().saturating_add(data.len()) as u64 > detect::MAX_FILE {
        r.error = Some("a folder's file arrived out of order or over its size limit".into());
    } else {
        part.extend_from_slice(data);
        if done {
            let bytes = r.part.remove(digest).unwrap_or_default();
            if hex(&Sha256::digest(&bytes)) == digest {
                r.got.insert(digest.to_owned(), bytes);
            } else {
                r.error = Some("a folder's file did not match its digest".into());
            }
        }
    }
    if read_complete(r) && let Some(tx) = r.done.take() {
        let _ = tx.try_send(());
    }
}

/// What a person sees of a package for one agent: as installed, and its state on each machine it
/// went to.
pub async fn view(hub: &Hub, p: &Installed, agent_machine: Option<&str>) -> Value {
    let (names, _) = machines(hub);
    let machines: Vec<String> = match &p.scope {
        Scope::Mine { .. } => names.into_iter().collect(),
        Scope::Repo { machine, .. } => vec![machine.clone()],
        Scope::Agent { .. } => agent_machine.map(str::to_owned).into_iter().collect(),
    };
    let mut on = Vec::new();
    for m in machines.into_iter().filter(|_| p.on) {
        let states: Vec<State> = hub.store.kv_get(state_key(&m)).await.ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let s = states.into_iter().find(|s| s.id == p.id);
        let state = match &s {
            Some(s) if s.sha == p.sha => s.state.clone(),
            // What the machine last said is of an older commit, or nothing yet.
            _ => "fetching".into(),
        };
        on.push(json!({"machine": if m == "local" { "server".to_owned() } else { hub.shown(&m) }, "state": state, "note": s.as_ref().map(|s| s.note.clone()).unwrap_or_default(), "log": s.as_ref().map(|s| s.log.clone()).unwrap_or_default()}));
    }
    json!({
        "id": p.id,
        "name": p.package.name,
        "version": p.package.version,
        "description": p.package.description,
        "source": p.source,
        "sha": p.sha,
        "shape": p.package.shape.word(),
        "skills": p.package.skills.iter().filter(|_| p.picks.skills).collect::<Vec<_>>(),
        "agents": p.package.agents.iter().filter(|_| p.picks.agents).collect::<Vec<_>>(),
        "hooks": if p.picks.hooks { p.package.hooks.clone() } else { Vec::new() },
        "mcp": if p.picks.mcp { p.package.mcp.clone() } else { Vec::new() },
        "setup": if p.picks.setup && p.picks.skills { p.package.setup.clone() } else { Vec::new() },
        "needs": if p.picks.skills { p.package.needs.clone() } else { Vec::new() },
        "scope": p.scope.word(),
        "on": p.on,
        "update": p.update,
        "machines": on,
        "by": p.by,
    })
}

/// What a draft found, as the page shows it to install.
pub fn draft_view(d: &Draft) -> Value {
    json!({
        "draft": d.id,
        "name": d.package.name,
        "version": d.package.version,
        "license": d.package.license,
        "description": d.package.description,
        "shape": d.package.shape.word(),
        "source": d.source,
        "sha": d.sha,
        "skills": d.package.skills,
        "agents": d.package.agents,
        "hooks": d.package.hooks,
        "mcp": d.package.mcp,
        "setup": d.package.setup,
        "needs": d.package.needs,
    })
}

/// The skills installed for session `s` (`Spec::skills` as it starts): its plugins' folder holds
/// them once its machine has laid them out.
pub async fn of(hub: &Hub, s: &At) -> Vec<String> {
    all(hub).await.map(|pkgs| for_session(&pkgs, s)).unwrap_or_default()
}

/// Read a SKILL.md of an installed package: its words, from kiln's store.
pub async fn read_skill(hub: &Hub, p: &Installed, skill: &str) -> R<String> {
    let path = format!("skills/{skill}/SKILL.md");
    let f = p.files.iter().find(|f| f.path == path).ok_or_else(|| Error::Missing(format!("{} has no skill {skill}", p.package.name)))?;
    let (_, bytes) = hub.blobs.blobs.get(&blob_key(&f.sha256), None).await.map_err(|e| Error::Other(anyhow::anyhow!("{e}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(id: &str, scope: Scope, at: i64) -> Installed {
        Installed {
            id: id.into(),
            source: Source::default(),
            sha: "s".into(),
            package: Package { name: id.into(), version: String::new(), description: String::new(), license: String::new(), shape: detect::Shape::SkillFolders, skills: Vec::new(), agents: Vec::new(), hooks: Vec::new(), mcp: Vec::new(), setup: Vec::new(), needs: Vec::new(), files: Vec::new() },
            picks: Picks::default(),
            files: Vec::new(),
            scope,
            by: "ada".into(),
            on: true,
            at,
            updated: at,
            update: None,
            checked: at,
        }
    }

    fn at(id: &str, machine: &str, root: &str, opener: &str) -> At {
        At { id: id.into(), machine: machine.into(), root: root.into(), opener: Some(opener.into()) }
    }

    #[test]
    fn w20_r01_scopes_say_which_sessions_have_which_packages() {
        let pkgs = vec![
            pkg("mine", Scope::Mine { opener: "owner".into() }, 1),
            pkg("one", Scope::Agent { agent: "a1".into() }, 2),
            pkg("repo", Scope::Repo { machine: "jupiter".into(), root: "/src/kiln".into() }, 3),
            Installed { on: false, ..pkg("off", Scope::Mine { opener: "owner".into() }, 4) },
        ];
        let a1 = at("a1", "adacachy", "/src/kiln", "owner");
        let a2 = at("a2", "jupiter", "/src/kiln/ui", "owner");
        let a3 = at("a3", "jupiter", "/src/kiln-other", "user:bo");
        assert_eq!(for_session(&pkgs, &a1), ["one", "mine"]);
        assert_eq!(for_session(&pkgs, &a2), ["repo", "mine"]);
        assert!(for_session(&pkgs, &a3).is_empty(), "another's agent, another folder");
        let machines = BTreeSet::from(["adacachy".to_owned(), "jupiter".to_owned(), "local".to_owned()]);
        let m = manifests(&pkgs, &[a1, a2, a3], &machines);
        let ids = |m: &str| m.to_owned();
        let names = |mm: &Manifest| mm.packages.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
        assert_eq!(m.get(&ids("adacachy")).map(names), Some(vec!["mine".to_owned(), "one".to_owned()]));
        assert_eq!(m.get(&ids("jupiter")).map(names), Some(vec!["mine".to_owned(), "repo".to_owned()]));
        assert_eq!(m.get(&ids("local")).map(names), Some(vec!["mine".to_owned()]), "a person's skills are ready on every machine");
        assert_eq!(m.get(&ids("jupiter")).map(|m| m.sessions.len()), Some(1));
    }

    #[test]
    fn w20_r01_unselected_skills_run_no_setup() {
        let mut p = pkg("agents-only", Scope::Agent { agent: "session".into() }, 1);
        p.package.setup.push(detect::Setup { skill: "python-tool".into(), stack: "Python".into(), command: "uv sync".into() });
        p.package.needs.push("uv".into());
        p.picks.skills = false;
        let placed = p.pkg();
        assert!(placed.setup.is_empty() && placed.needs.is_empty(), "agents-only selection ran a skill's setup");
    }

    #[test]
    fn w20_r01_folder_replies_are_bound_to_the_requested_runner() {
        let bytes = b"# skill";
        let digest = hex(&Sha256::digest(bytes));
        let req = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let files = vec![FileRef { path: "a/SKILL.md".into(), sha256: digest.clone(), size: bytes.len() as u64, exec: false }];
        READS.lock().insert(req, Reading { peer: 99, files: Some(files), ..Reading::default() });
        heard_file(100, req, &digest, 0, bytes, true);
        assert!(READS.lock().get(&req).is_some_and(|r| r.got.is_empty()));
        heard_file(99, req, &digest, 0, bytes, true);
        assert!(READS.lock().get(&req).is_some_and(read_complete));
        READS.lock().remove(&req);
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what a sync weighs (every package read, each machine's manifest made, the runners'
    /// written out) goes to the compute pool, not the shard that heard of what changed (a session starting, a runner
    /// connecting).
    #[test]
    fn g9_s3_a_sync_weighs_its_manifests_off_the_shard() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let asked = crate::rt::COMPUTE_ASKED.with(std::cell::Cell::get);
            sync(&hub).await;
            anyhow::ensure!(crate::rt::COMPUTE_ASKED.with(std::cell::Cell::get) > asked, "a sync's weighing goes to the compute pool");
            Ok(())
        })
    }

    #[test]
    fn w20_r01_drafts_keep_files_and_cannot_be_installed_by_someone_else() -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = crate::rt::block_on(async {
                let hub = crate::hub::tests::scratch_hub().await?;
                let obsolete = source::written("obsolete", "Previous draft.", "# Before").map_err(anyhow::Error::msg)?;
                let previous = look(&hub, "alice", Source::default(), String::new(), obsolete, None, "obsolete").await?
                    .map_err(|_| anyhow::anyhow!("expected one previous skill"))?;
                let mut files = source::written("notes", "Use for release notes.", "# Steps").map_err(anyhow::Error::msg)?;
                let skill_file = files.first_mut().ok_or_else(|| anyhow::anyhow!("no written skill"))?;
                skill_file.path = "notes/SKILL.md".into();
                files.push(File { path: "hooks/hooks.json".into(), bytes: br#"{"hooks":{"PostToolUse":[]}}"#.to_vec(), exec: false });
                let looked = look(&hub, "alice", Source { kind: "write".into(), ..Source::default() }, String::new(), files, None, "notes").await?;
                let d = looked.map_err(|_| anyhow::anyhow!("expected one skill"))?;
                let kept = draft(&hub, &d.id).await?;
                anyhow::ensure!(kept.files.len() == 2 && kept.package.name == "notes");
                for f in &previous.files {
                    anyhow::ensure!(hub.blobs.blobs.head(&blob_key(&f.sha256)).await.is_err(), "replaced draft bytes stayed");
                }
                let file = kept.files.first().ok_or_else(|| anyhow::anyhow!("missing file"))?;
                let (_, bytes) = hub.blobs.blobs.get(&blob_key(&file.sha256), None).await?;
                anyhow::ensure!(String::from_utf8_lossy(&bytes).contains("# Steps"));
                let denied = install(&hub, "bob", &d.id, Picks::default(), Scope::Agent { agent: "session".into() }).await;
                anyhow::ensure!(matches!(denied, Err(Error::Forbidden(_))), "another person installed a private draft");
                anyhow::ensure!(all(&hub).await?.is_empty());
                let scope = Scope::Agent { agent: "session".into() };
                let installed = install(&hub, "alice", &d.id, Picks::default(), scope).await?;
                let hook = kept.files.iter().find(|f| f.path == "hooks/hooks.json").ok_or_else(|| anyhow::anyhow!("no hook"))?;
                anyhow::ensure!(hub.blobs.blobs.head(&blob_key(&hook.sha256)).await.is_err(), "unselected hook bytes stayed after installation");
                anyhow::ensure!(read_skill(&hub, &installed, "notes").await?.contains("# Steps"));
                let retried = set(&hub, &installed.id, true).await?;
                anyhow::ensure!(retried.updated > installed.updated && retried.pkg().key() != installed.pkg().key());
                let off = set(&hub, &installed.id, false).await?;
                anyhow::ensure!(!off.on);
                let off_view = view(&hub, &off, Some("local")).await;
                anyhow::ensure!(off_view.get("machines").and_then(Value::as_array).is_some_and(Vec::is_empty), "an off package reported active placements");
                remove(&hub, &installed.id).await?;
                anyhow::ensure!(all(&hub).await?.is_empty());
                for f in &installed.files {
                    anyhow::ensure!(matches!(hub.blobs.blobs.head(&blob_key(&f.sha256)).await, Err(kiln_seams::blobs::Refusal::NotFound(_))), "unreferenced bytes stayed after removal");
                }
                anyhow::Ok(())
            });
            let _ = tx.send(result);
        });
        rx.recv_timeout(std::time::Duration::from_secs(120)).map_err(anyhow::Error::from)?
    }

    #[test]
    fn w20_r01_home_folders_expand() {
        assert_eq!(expand("/abs/x"), std::path::PathBuf::from("/abs/x"));
        assert_eq!(expand("~/skills"), crate::os::home_dir().join("skills"));
        assert_eq!(expand("~other/x"), std::path::PathBuf::from("~other/x"));
    }
}
