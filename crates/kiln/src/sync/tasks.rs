//! Putting repositories on machines (`docs/sync.md`, "Repositories on machines"): a clone, a sync
//! or a move is a **task** for each machine it goes to, the tasks started together a **batch**,
//! driven from here. Nothing travels but over kiln's own links: commits as a git bundle (made from
//! kiln's mirror of a repository it builds, or by the machine that has it) fetched over HTTP from
//! the server, a working tree through the repo's sync group.
//!
//! - **Clone**: each machine clones the bundle (or the remote itself) into its folder.
//! - **Sync**: each new folder is cloned, then joins the repo's sync group as a folder that takes
//!   the group's files ([`crate::sync::Copy::adopt`]): the source's working tree, uncommitted work
//!   included, with no conflict copies. A repo not synced yet starts its group from the source
//!   folder (or, cloned from kiln's mirror, from the first new folder).
//! - **Move**: as sync, to one machine; once its folder is in step, the old copy leaves the group
//!   (a group made for the move alone stops), and with `delete` the old folder goes.
//!
//! A task's machine says how its part goes ([`TaskNews`]); a task whose machine stays silent, or
//! a batch the server's restart cut short, fails and can be started again: what is done already
//! (a clone there, a copy in the group) is found and kept, never done twice.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::places::Row;
use super::wire::{CloneOrder, Msg, TaskNews};
use crate::hub::Hub;

/// A machine that says nothing of its part for this long has stopped (its runner sends news every
/// few seconds while it works).
const QUIET: Duration = Duration::from_secs(180);
/// How long a machine may be away mid-task, and how long a copy may take to come in step.
const AWAY: Duration = Duration::from_secs(600);
const CATCH_UP: Duration = Duration::from_secs(6 * 3600);
/// Tasks kept once over: the newest, for this long.
const KEEP: usize = 200;
const KEEP_FOR: i64 = 7 * 86_400;
/// Notes one task keeps.
const NOTES: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Clone,
    Sync,
    Move,
    /// A place given up: a copy leaves its group, a clone is forgotten, its folder deleted if asked.
    Remove,
}

/// Where a repository's commits come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    /// kiln's mirror of a repository it builds: a clean checkout of a branch.
    Kiln { repo: String },
    /// A machine's folder: its commits by git, and with sync its working tree too.
    Machine { machine: String, path: String },
    /// A remote each machine clones from itself.
    Url { url: String },
}

/// What happened, when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub at: i64,
    pub text: String,
}

/// One machine's part of a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub batch: String,
    pub kind: Kind,
    /// The repository: its key, as people say it.
    pub key: String,
    pub name: String,
    pub from: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Where it goes (for a removal: the place given up).
    pub machine: String,
    pub path: String,
    /// A move deletes the old folder; a removal deletes the folder.
    #[serde(default)]
    pub delete: bool,
    /// `queued`, `bundling`, `fetching`, `cloning`, `joining`, `catching up`, `leaving`,
    /// `deleting`, `done` or `failed`.
    pub state: String,
    /// What it is doing, in words.
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What it did, oldest first.
    #[serde(default)]
    pub notes: Vec<Note>,
    /// Unix seconds.
    pub created: i64,
    pub updated: i64,
    /// The synced repo it joined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced: Option<String>,
    /// What its clone checked out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// A sync's: what the copies keep the same.
    #[serde(default, skip_serializing_if = "super::Mode::is_files")]
    pub mode: super::Mode,
}

impl Task {
    pub fn over(&self) -> bool {
        self.state == "done" || self.state == "failed"
    }
}

/// A bundle for a batch: made by the server or sent by `from`, fetched by `to`.
struct Bundle {
    from: Option<String>,
    to: HashSet<String>,
    hash: Option<String>,
    size: u64,
}

#[derive(Default)]
struct Inner {
    tasks: Vec<Task>,
    /// What each machine last said of a task or batch, and when.
    news: HashMap<(String, String), (TaskNews, Instant)>,
    bundles: HashMap<String, Bundle>,
}

/// Every task, under way or done lately (`<data>/sync/tasks.json`), and the bundles batches hold
/// (`<data>/sync/bundles`).
pub struct Tasks {
    file: PathBuf,
    dir: PathBuf,
    inner: Mutex<Inner>,
    changed: event_listener::Event,
}

/// What to do: from where, to which machines and folders.
#[derive(Debug, Clone, Deserialize)]
pub struct Ask {
    pub kind: Kind,
    /// The repository's key (`crate::sync::places::Row::key`).
    pub key: String,
    pub from: Option<Source>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub to: Vec<Target>,
    #[serde(default)]
    pub delete: bool,
    /// A sync's: what the copies keep the same (their files, their git refs, or both).
    #[serde(default)]
    pub mode: super::Mode,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub machine: String,
    /// A whole path, or `~/…` on that machine.
    pub path: String,
}

/// Forgets the tasks over for longer than they're kept, and the oldest past the most kept: whether
/// it forgot any.
fn prune(tasks: &mut Vec<Task>, now: i64) -> bool {
    let before = tasks.len();
    tasks.retain(|t| !t.over() || now - t.updated < KEEP_FOR);
    let over = tasks.iter().filter(|t| t.over()).count();
    if over > KEEP {
        let mut old: Vec<(i64, String)> = tasks.iter().filter(|t| t.over()).map(|t| (t.updated, t.id.clone())).collect();
        old.sort();
        let drop: HashSet<String> = old.into_iter().take(over - KEEP).map(|(_, id)| id).collect();
        tasks.retain(|t| !drop.contains(&t.id));
    }
    tasks.len() != before
}

impl Tasks {
    /// The tasks kept in `dir`; any a restart cut short failed.
    pub fn open(dir: &Path) -> Self {
        let file = dir.join("tasks.json");
        let mut tasks: Vec<Task> = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let now = crate::util::now_unix();
        let mut cut = false;
        for t in tasks.iter_mut().filter(|t| !t.over()) {
            t.state = "failed".into();
            t.error = Some("the server restarted while it was under way: start it again".into());
            t.updated = now;
            cut = true;
        }
        let pruned = prune(&mut tasks, now);
        let bundles = dir.join("bundles");
        let _ = std::fs::remove_dir_all(&bundles);
        let _ = std::fs::create_dir_all(&bundles);
        let me = Self { file, dir: bundles, inner: Mutex::new(Inner { tasks, ..Inner::default() }), changed: event_listener::Event::new() };
        // Written again only when the restart failed one under way, or forgot ones kept long
        // enough: unchanged, the file stands.
        if cut || pruned {
            me.save();
        }
        me
    }

    /// The tasks, newest first.
    pub fn list(&self) -> Vec<Task> {
        let mut out = self.inner.lock().tasks.clone();
        out.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
        out
    }

    pub fn get(&self, id: &str) -> Option<Task> {
        self.inner.lock().tasks.iter().find(|t| t.id == id).cloned()
    }

    /// A task over is forgotten.
    pub fn dismiss(&self, id: &str) -> bool {
        let gone = {
            let mut inner = self.inner.lock();
            let before = inner.tasks.len();
            inner.tasks.retain(|t| t.id != id || !t.over());
            before != inner.tasks.len()
        };
        if gone {
            self.save();
        }
        gone
    }

    /// What a machine said of its part of a task (or of a batch's bundle).
    pub fn heard(&self, machine: &str, news: TaskNews) {
        self.inner.lock().news.insert((news.task.clone(), machine.to_owned()), (news, Instant::now()));
        self.changed.notify(usize::MAX);
    }

    fn news(&self, id: &str, machine: &str) -> Option<(TaskNews, Instant)> {
        self.inner.lock().news.get(&(id.to_owned(), machine.to_owned())).cloned()
    }

    fn save(&self) {
        let bytes = {
            let mut inner = self.inner.lock();
            prune(&mut inner.tasks, crate::util::now_unix());
            serde_json::to_vec(&inner.tasks).unwrap_or_default()
        };
        if let Err(e) = super::group::save_atomic(&self.file, &bytes) {
            tracing::warn!(error = %e, "tasks could not be saved");
        }
    }

    /// Changes a task, keeps it, and tells the console.
    fn update(&self, hub: &Hub, id: &str, f: impl FnOnce(&mut Task)) {
        let found = {
            let mut inner = self.inner.lock();
            match inner.tasks.iter_mut().find(|t| t.id == id) {
                Some(t) => {
                    let before = (t.state.clone(), t.detail.clone(), t.error.clone());
                    f(t);
                    t.updated = crate::util::now_unix();
                    if (t.state.clone(), t.detail.clone(), t.error.clone()) != before {
                        let text = match &t.error {
                            Some(e) if t.state == "failed" => format!("failed: {e}"),
                            _ => format!("{}: {}", t.state, t.detail),
                        };
                        if t.notes.last().is_none_or(|n| n.text != text) {
                            t.notes.push(Note { at: t.updated, text });
                            if t.notes.len() > NOTES {
                                t.notes.remove(0);
                            }
                        }
                    }
                    true
                }
                None => false,
            }
        };
        if found {
            self.save();
            self.changed.notify(usize::MAX);
            hub.emit(&crate::wire::events::ServerEvent::Places);
        }
    }

    fn set(&self, hub: &Hub, id: &str, state: &str, detail: impl Into<String>) {
        let detail = detail.into();
        self.update(hub, id, |t| {
            if !t.over() {
                state.clone_into(&mut t.state);
                t.detail = detail;
            }
        });
    }

    fn fail(&self, hub: &Hub, id: &str, error: impl Into<String>) {
        let error = error.into();
        self.update(hub, id, |t| {
            if !t.over() {
                t.state = "failed".into();
                t.error = Some(error);
            }
        });
    }

    fn done(&self, hub: &Hub, id: &str, detail: impl Into<String>) {
        let detail = detail.into();
        self.update(hub, id, |t| {
            if !t.over() {
                t.state = "done".into();
                t.detail = detail;
            }
        });
    }

    // -------------------------------------------------------------------------- bundles

    /// Where a batch's bundle is kept.
    pub fn bundle_file(&self, batch: &str) -> Option<PathBuf> {
        let safe = !batch.is_empty() && batch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        safe.then(|| self.dir.join(format!("{batch}.bundle")))
    }

    /// Whether `machine` is to send the batch's bundle, and hasn't.
    pub fn may_send(&self, batch: &str, machine: &str) -> bool {
        self.inner.lock().bundles.get(batch).is_some_and(|b| b.from.as_deref() == Some(machine) && b.hash.is_none())
    }

    /// The bundle came, whole: its SHA-256 and size.
    pub fn sent(&self, batch: &str, hash: String, size: u64) {
        if let Some(b) = self.inner.lock().bundles.get_mut(batch) {
            b.hash = Some(hash);
            b.size = size;
        }
        self.changed.notify(usize::MAX);
    }

    /// The batch's bundle, its SHA-256 and size, for a machine it goes to.
    pub fn may_fetch(&self, batch: &str, machine: &str) -> Option<(PathBuf, String, u64)> {
        let inner = self.inner.lock();
        let b = inner.bundles.get(batch).filter(|b| b.to.contains(machine))?;
        Some((self.bundle_file(batch)?, b.hash.clone()?, b.size))
    }

    pub fn bundled(&self, batch: &str) -> Option<(String, u64)> {
        self.inner.lock().bundles.get(batch).and_then(|b| b.hash.clone().map(|h| (h, b.size)))
    }

    /// A bundle `from` sends to batch `batch` for the machines `to` (`crate::sync::share`): kept
    /// as it is when it's there already, its machines then `to`.
    pub fn open_bundle(&self, batch: &str, from: &str, to: HashSet<String>) {
        let mut inner = self.inner.lock();
        match inner.bundles.get_mut(batch) {
            Some(b) => b.to = to,
            None => {
                inner.bundles.insert(batch.to_owned(), Bundle { from: Some(from.to_owned()), to, hash: None, size: 0 });
            }
        }
    }

    /// `machine` may fetch batch `batch`'s bundle too.
    pub fn allow(&self, batch: &str, machine: &str) {
        if let Some(b) = self.inner.lock().bundles.get_mut(batch) {
            b.to.insert(machine.to_owned());
        }
    }

    pub fn drop_bundle(&self, batch: &str) {
        self.inner.lock().bundles.remove(batch);
        if let Some(f) = self.bundle_file(batch) {
            let _ = std::fs::remove_file(f);
        }
    }
}

// ------------------------------------------------------------------------------ starting

/// A batch, as its driver needs it.
struct Batch {
    id: String,
    kind: Kind,
    key: String,
    name: String,
    from: Source,
    branch: Option<String>,
    /// The tasks, and each one's machine and folder.
    tasks: Vec<(String, String, String)>,
    delete: bool,
    /// What a clone's `origin` is.
    origin: Option<String>,
    /// The source folder is a git repository (a clone needs its bundle), and a synced copy (its
    /// synced repo and member).
    source_git: bool,
    source_copy: Option<(String, u64)>,
    /// The repository's synced repo, when it has one.
    synced: Option<String>,
    /// What a new synced repo keeps the same.
    mode: super::Mode,
}

fn row_of(view: &super::places::View, key: &str) -> Result<Row, String> {
    view.repos.iter().find(|r| r.key == key).cloned().ok_or_else(|| "no such repository".to_owned())
}

/// Starts what `ask` says: its tasks, under way at once.
pub async fn start(hub: &Arc<Hub>, ask: Ask) -> Result<Vec<Task>, String> {
    let view = super::places::now(hub).await.map_err(|e| format!("{e:#}"))?;
    let row = row_of(&view, &ask.key)?;
    let machine = |name: &str| view.machines.iter().find(|m| m.name == name);
    let place = |m: &str, p: &str| row.places.iter().find(|x| x.machine == m && x.path == p);
    if ask.kind == Kind::Remove {
        return remove(hub, &row, &ask, &view).await;
    }
    anyhow_free(!ask.to.is_empty(), "choose a machine for it to go to")?;
    if ask.kind == Kind::Move {
        anyhow_free(ask.to.len() == 1, "a repository moves to one machine")?;
    }
    let from = match ask.from.clone() {
        Some(f) => f,
        None => match (&row.built, row.places.iter().find(|p| p.online && p.copy.as_ref().is_some_and(|c| c.state == "in step"))) {
            (_, Some(p)) => Source::Machine { machine: p.machine.clone(), path: p.path.clone() },
            (Some(b), None) => Source::Kiln { repo: b.clone() },
            (None, None) => return Err("choose where it comes from".into()),
        },
    };
    let (origin, source_git, source_copy) = match &from {
        Source::Kiln { repo } => {
            anyhow_free(row.built.as_deref() == Some(repo.as_str()), "kiln doesn't build that repository")?;
            (row.origin.clone(), true, None)
        }
        Source::Machine { machine: m, path: p } => {
            let src = place(m, p).ok_or_else(|| format!("{m} has no copy of {} at {p}", row.name))?;
            let mv = machine(m).ok_or_else(|| format!("no machine named {m}"))?;
            anyhow_free(mv.connected, &format!("{m} is offline"))?;
            let git = src.git.is_some();
            if git {
                anyhow_free(mv.can_clone, &format!("{m}'s runner is too old to bundle repositories: upgrade it"))?;
            }
            let copy = src.copy.as_ref().filter(|c| !c.moving).map(|c| (c.repo.clone(), c.member));
            (src.git.as_ref().and_then(|g| g.origin.clone()), git, copy)
        }
        Source::Url { url } => {
            anyhow_free(!url.trim().is_empty(), "a remote to clone from")?;
            anyhow_free(ask.kind != Kind::Move, "a move starts from a machine's folder")?;
            (Some(url.trim().to_owned()), true, None)
        }
    };
    if ask.kind == Kind::Move {
        anyhow_free(matches!(from, Source::Machine { .. }), "a move starts from a machine's folder")?;
    }
    if ask.kind == Kind::Clone {
        anyhow_free(source_git, "a clone needs a git repository: that folder isn't one (sync it instead)")?;
    }
    let mut targets: Vec<(String, String)> = Vec::new();
    for t in &ask.to {
        let m = machine(&t.machine).ok_or_else(|| format!("no machine named {}", t.machine))?;
        anyhow_free(m.connected, &format!("{} is offline", t.machine))?;
        if source_git {
            anyhow_free(m.can_clone, &format!("{}'s runner is too old to clone repositories: upgrade it", t.machine))?;
        }
        if ask.kind != Kind::Clone {
            anyhow_free(m.can_sync, &format!("{}'s runner is too old to sync: upgrade it", t.machine))?;
        }
        let path = hub.places.resolve(&t.machine, &t.path).ok_or_else(|| {
            if t.path.trim().starts_with('~') {
                format!("{} hasn't said where its home is yet: give a whole path", t.machine)
            } else {
                format!("{:?} is no whole path on {}", t.path, t.machine)
            }
        })?;
        // A folder there already is joined only when kiln made it for this repository (a task
        // tried before): a clone of its own, or a copy of its sync.
        if let Some(there) = place(&t.machine, &path) {
            let ours = there.kind == "clone" || there.git.as_ref().is_some_and(|g| g.kiln) || there.copy.as_ref().is_some_and(|c| c.moving || row.synced.as_deref() == Some(c.repo.as_str()));
            anyhow_free(ours && ask.kind != Kind::Clone, &format!("{} is at {path} on {} already", row.name, t.machine))?;
        }
        anyhow_free(!targets.contains(&(t.machine.clone(), path.clone())), &format!("{}:{path} is chosen twice", t.machine))?;
        if let Source::Machine { machine: m, path: p } = &from {
            anyhow_free(!(m == &t.machine && p == &path), "that is where it is already")?;
        }
        targets.push((t.machine.clone(), path));
    }
    let batch = crate::util::new_id();
    let now = crate::util::now_unix();
    let tasks: Vec<Task> = targets
        .iter()
        .map(|(m, p)| Task {
            id: crate::util::new_id(),
            batch: batch.clone(),
            kind: ask.kind,
            key: row.key.clone(),
            name: row.name.clone(),
            from: from.clone(),
            branch: ask.branch.clone().filter(|b| !b.trim().is_empty()),
            machine: m.clone(),
            path: p.clone(),
            delete: ask.delete && ask.kind == Kind::Move,
            state: "queued".into(),
            detail: "starting".into(),
            error: None,
            notes: Vec::new(),
            created: now,
            updated: now,
            synced: None,
            head: None,
            mode: if ask.kind == Kind::Sync { ask.mode } else { super::Mode::Files },
        })
        .collect();
    {
        let mut inner = hub.tasks.inner.lock();
        inner.tasks.extend(tasks.iter().cloned());
    }
    hub.tasks.save();
    hub.emit(&crate::wire::events::ServerEvent::Places);
    let b = Batch {
        id: batch,
        kind: ask.kind,
        key: row.key.clone(),
        name: row.name.clone(),
        from,
        branch: ask.branch.filter(|b| !b.trim().is_empty()),
        tasks: tasks.iter().map(|t| (t.id.clone(), t.machine.clone(), t.path.clone())).collect(),
        delete: ask.delete && ask.kind == Kind::Move,
        origin,
        source_git,
        source_copy,
        synced: row.synced.clone(),
        mode: if ask.kind == Kind::Sync { ask.mode } else { super::Mode::Files },
    };
    let h = hub.clone();
    crate::rt::spawn(async move { drive(h, b).await });
    Ok(tasks)
}

fn anyhow_free(ok: bool, why: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why.to_owned()) }
}

/// A place given up (`ask.from`, a machine's folder): a copy leaves its group, a clone kiln made
/// is forgotten, and with `delete` the folder goes.
async fn remove(hub: &Arc<Hub>, row: &Row, ask: &Ask, view: &super::places::View) -> Result<Vec<Task>, String> {
    let Some(Source::Machine { machine, path }) = ask.from.clone() else { return Err("which place to give up".into()) };
    let place = row.places.iter().find(|p| p.machine == machine && p.path == path).cloned().ok_or_else(|| format!("{machine} has no copy of {} at {path}", row.name))?;
    let m = view.machines.iter().find(|m| m.name == machine);
    if ask.delete || place.kind != "copy" {
        anyhow_free(m.is_some_and(|m| m.connected), &format!("{machine} is offline"))?;
        anyhow_free(m.is_some_and(|m| m.can_clone), &format!("{machine}'s runner is too old for that: upgrade it"))?;
    }
    anyhow_free(ask.delete || place.kind != "found", "kiln didn't make that folder: it can only delete it")?;
    let now = crate::util::now_unix();
    let task = Task {
        id: crate::util::new_id(),
        batch: crate::util::new_id(),
        kind: Kind::Remove,
        key: row.key.clone(),
        name: row.name.clone(),
        from: Source::Machine { machine: machine.clone(), path: path.clone() },
        branch: None,
        machine: machine.clone(),
        path: path.clone(),
        delete: ask.delete,
        state: "queued".into(),
        detail: "starting".into(),
        error: None,
        notes: Vec::new(),
        created: now,
        updated: now,
        synced: None,
        head: None,
        mode: super::Mode::Files,
    };
    hub.tasks.inner.lock().tasks.push(task.clone());
    hub.tasks.save();
    let (h, t) = (hub.clone(), task.clone());
    let copy = place.copy.clone();
    let origin = place.git.as_ref().and_then(|g| g.origin.clone());
    crate::rt::spawn(async move {
        let out: Result<String, String> = async {
            if let Some(c) = &copy {
                h.tasks.set(&h, &t.id, "leaving", format!("{machine} stops syncing it"));
                h.sync.ask(super::server::Ask::RemoveCopy { repo: c.repo.clone(), member: c.member }).await?;
            }
            if t.delete {
                delete_on(&h, &t.id, &machine, &path, origin.as_deref()).await?;
                Ok(format!("{path} deleted on {machine}"))
            } else {
                h.sync.send(&machine, Msg::Forget { path: path.clone() });
                h.sync.send(&machine, Msg::Look);
                Ok(format!("kiln no longer keeps it; its files stay in {path}"))
            }
        }
        .await;
        match out {
            Ok(d) => h.tasks.done(&h, &t.id, d),
            Err(e) => h.tasks.fail(&h, &t.id, e),
        }
    });
    Ok(vec![task])
}

/// Starts a failed task again, with what it asked; what it did already is found and kept.
pub async fn retry(hub: &Arc<Hub>, id: &str) -> Result<Vec<Task>, String> {
    let t = hub.tasks.get(id).ok_or_else(|| "no such task".to_owned())?;
    anyhow_free(t.state == "failed", "only a failed task is started again")?;
    let ask = Ask {
        kind: t.kind,
        key: t.key.clone(),
        from: Some(t.from.clone()),
        branch: t.branch.clone(),
        to: if t.kind == Kind::Remove { Vec::new() } else { vec![Target { machine: t.machine.clone(), path: t.path.clone() }] },
        delete: t.delete,
        mode: t.mode,
    };
    let started = start(hub, ask).await?;
    hub.tasks.dismiss(id);
    Ok(started)
}

// ------------------------------------------------------------------------------ driving

async fn drive(hub: Arc<Hub>, b: Batch) {
    if let Err(e) = drive_batch(&hub, &b).await {
        for (id, _, _) in &b.tasks {
            hub.tasks.fail(&hub, id, e.clone());
        }
    }
    hub.tasks.drop_bundle(&b.id);
    for m in b.tasks.iter().map(|(_, m, _)| m).collect::<HashSet<_>>() {
        hub.sync.send(m, Msg::Look);
    }
    if let Source::Machine { machine, .. } = &b.from {
        hub.sync.send(machine, Msg::Look);
    }
}

/// Where the commits come from, as each machine takes them.
enum Commits {
    /// The batch's bundle on the server.
    Bundle { hash: String, size: u64 },
    Url(String),
    /// A folder that is no git repository: sync fills the new folders.
    Folder,
}

async fn drive_batch(hub: &Arc<Hub>, b: &Batch) -> Result<(), String> {
    let ids: Vec<String> = b.tasks.iter().map(|(id, _, _)| id.clone()).collect();
    let to: HashSet<String> = b.tasks.iter().map(|(_, m, _)| m.clone()).collect();
    // 1. The commits.
    let source = match &b.from {
        Source::Kiln { repo } => {
            for id in &ids {
                hub.tasks.set(hub, id, "bundling", format!("kiln bundles its mirror of {}", b.name));
            }
            hub.tasks.inner.lock().bundles.insert(b.id.clone(), Bundle { from: None, to, hash: None, size: 0 });
            let (hash, size) = mirror_bundle(hub, repo, &b.id).await.map_err(|e| format!("bundling kiln's mirror: {e:#}"))?;
            hub.tasks.sent(&b.id, hash.clone(), size);
            Commits::Bundle { hash, size }
        }
        Source::Machine { machine, path } if b.source_git => {
            for id in &ids {
                hub.tasks.set(hub, id, "bundling", format!("{machine} bundles its commits"));
            }
            hub.tasks.inner.lock().bundles.insert(b.id.clone(), Bundle { from: Some(machine.clone()), to, hash: None, size: 0 });
            hub.sync.send(machine, Msg::Bundle { task: b.id.clone(), path: path.clone() });
            wait_news(hub, &b.id, machine, &ids).await?;
            let (hash, size) = hub.tasks.bundled(&b.id).ok_or_else(|| format!("{machine} said it sent the bundle, and none came"))?;
            Commits::Bundle { hash, size }
        }
        Source::Machine { .. } => Commits::Folder,
        Source::Url { url } => Commits::Url(url.clone()),
    };
    // 2. Each machine's clone, all at once. A clone alone is over on a machine as soon as that
    // machine's is: a slow one keeps no other waiting.
    if b.kind == Kind::Clone {
        let source = &source;
        futures::future::join_all(b.tasks.iter().map(|(id, m, p)| async move {
            match clone_on(hub, b, id, m, p, source).await {
                Ok((head, _)) => {
                    hub.tasks.update(hub, id, |t| t.head.clone_from(&head));
                    hub.tasks.done(hub, id, format!("cloned into {p} on {m}"));
                }
                Err(e) => hub.tasks.fail(hub, id, e),
            }
        }))
        .await;
        return Ok(());
    }
    let clones = futures::future::join_all(b.tasks.iter().map(|(id, m, p)| clone_on(hub, b, id, m, p, &source))).await;
    // 3. The group each new folder joins.
    let cloned: Vec<(&String, &String, &String, bool)> = b
        .tasks
        .iter()
        .zip(clones)
        .filter_map(|((id, m, p), r)| match r {
            // Only a folder this batch cloned takes the group's files; one kiln made before joins
            // as it is, so nothing changed in it since is lost.
            Ok((head, fresh)) => {
                hub.tasks.update(hub, id, |t| t.head.clone_from(&head));
                Some((id, m, p, fresh))
            }
            Err(e) => {
                hub.tasks.fail(hub, id, e);
                None
            }
        })
        .collect();
    if cloned.is_empty() {
        return Ok(());
    }
    let mut joined: Vec<(&String, &String, &String, String, u64)> = Vec::new();
    let (group, transient) = match (&b.from, &b.source_copy) {
        // The source is a copy: its group.
        (Source::Machine { machine, .. }, Some((repo, member))) => {
            wait_in_step(hub, repo, *member, &ids, &format!("the copy on {machine}")).await?;
            (repo.clone(), false)
        }
        // A move of a folder not synced: a group for the move alone, from it.
        (Source::Machine { machine, path }, _) if b.kind == Kind::Move => (seed(hub, b, machine, path, true, &ids).await?, true),
        // A sync, of a repository synced already: its group (the clones take its files).
        _ if b.synced.is_some() => (b.synced.clone().unwrap_or_default(), false),
        // A sync of a folder not synced yet: its group starts from it.
        (Source::Machine { machine, path }, _) => (seed(hub, b, machine, path, false, &ids).await?, false),
        // A sync of clones from kiln's mirror or a remote: the first starts the group.
        _ => {
            let Some(&(id, m, p, _)) = cloned.first() else { return Ok(()) };
            hub.tasks.set(hub, id, "joining", format!("{} starts syncing", m));
            let repo = hub.sync.ask(super::server::Ask::Create { name: unique_name(hub, &b.name), copies: vec![(m.clone(), p.clone())], exclude: Vec::new(), mode: b.mode, transient: None }).await?;
            let member = member_at(hub, &repo, m, p).ok_or_else(|| "the new synced repo has no copy".to_owned())?;
            joined.push((id, m, p, repo.clone(), member));
            (repo, false)
        }
    };
    // 4. Each new folder joins, and catches up.
    let first = joined.first().map(|j| j.0.clone());
    for &(id, m, p, cloned_here) in &cloned {
        if first.as_ref() == Some(id) {
            continue;
        }
        hub.tasks.set(hub, id, "joining", format!("{m} joins the sync"));
        let member = match member_at(hub, &group, m, p) {
            Some(member) => member,
            None => match hub.sync.ask(super::server::Ask::AddCopy { repo: group.clone(), machine: m.clone(), path: p.clone(), adopt: cloned_here }).await {
                Ok(member) => member.parse().unwrap_or(0),
                Err(e) => {
                    hub.tasks.fail(hub, id, e);
                    continue;
                }
            },
        };
        joined.push((id, m, p, group.clone(), member));
    }
    let caught = futures::future::join_all(joined.iter().map(|(id, m, _, repo, member)| async move {
        hub.tasks.update(hub, id, |t| t.synced = Some(repo.clone()));
        wait_in_step(hub, repo, *member, std::slice::from_ref(*id), &format!("the copy on {m}")).await
    }))
    .await;
    // 5. Synced: done. Moved: the old copy leaves, the old folder goes if asked.
    for ((id, m, p, _, _), r) in joined.iter().zip(caught) {
        if let Err(e) = r {
            hub.tasks.fail(hub, id, e);
            continue;
        }
        if b.kind == Kind::Sync {
            hub.tasks.done(hub, id, format!("in step at {p} on {m}"));
            continue;
        }
        let Source::Machine { machine: om, path: op } = &b.from else { continue };
        let out = leave(hub, b, id, &group, transient, om, op).await;
        match out {
            Ok(()) => hub.tasks.done(hub, id, format!("moved to {p} on {m}")),
            Err(e) => hub.tasks.fail(hub, id, e),
        }
    }
    Ok(())
}

/// The old copy of a move leaves the group once it, too, is in step (a group made for the move
/// stops); then, if asked, its folder goes.
async fn leave(hub: &Arc<Hub>, b: &Batch, id: &str, group: &str, transient: bool, machine: &str, path: &str) -> Result<(), String> {
    if let Some(member) = member_at(hub, group, machine, path) {
        wait_in_step(hub, group, member, &[id.to_owned()], &format!("the old copy on {machine}")).await?;
        hub.tasks.set(hub, id, "leaving", format!("{machine} stops syncing it"));
        if transient {
            hub.sync.ask(super::server::Ask::Delete { repo: group.to_owned() }).await?;
        } else {
            hub.sync.ask(super::server::Ask::RemoveCopy { repo: group.to_owned(), member }).await?;
        }
    }
    if b.delete {
        delete_on(hub, id, machine, path, b.origin.as_deref()).await?;
    }
    Ok(())
}

/// Deletes a folder on its machine: the runner checks it is the repository (`origin`) and nowhere
/// it must not delete.
async fn delete_on(hub: &Arc<Hub>, id: &str, machine: &str, path: &str, origin: Option<&str>) -> Result<(), String> {
    hub.tasks.set(hub, id, "deleting", format!("{machine} deletes {path}"));
    hub.sync.send(machine, Msg::Delete { task: id.to_owned(), path: path.to_owned(), origin: origin.map(str::to_owned) });
    wait_news(hub, id, machine, &[id.to_owned()]).await.map(|_| ())
}

/// A group from a folder (`transient`: for a move alone), once that copy is in step.
async fn seed(hub: &Arc<Hub>, b: &Batch, machine: &str, path: &str, transient: bool, ids: &[String]) -> Result<String, String> {
    for id in ids {
        hub.tasks.set(hub, id, "joining", format!("{machine} starts syncing {path}"));
    }
    let name = if transient { unique_name(hub, &format!("{} (moving)", b.name)) } else { unique_name(hub, &b.name) };
    let mode = if transient { super::Mode::Files } else { b.mode };
    let repo = hub.sync.ask(super::server::Ask::Create { name, copies: vec![(machine.to_owned(), path.to_owned())], exclude: Vec::new(), mode, transient: transient.then(|| b.id.clone()) }).await?;
    let member = member_at(hub, &repo, machine, path).ok_or_else(|| "the new synced repo has no copy".to_owned())?;
    wait_in_step(hub, &repo, member, ids, &format!("the copy on {machine}")).await?;
    Ok(repo)
}

/// A synced repo name no other has: `name`, else `name (2)`, ….
fn unique_name(hub: &Hub, name: &str) -> String {
    let taken: HashSet<String> = hub.sync.view().repos.into_iter().map(|r| r.name).collect();
    if !taken.contains(name) {
        return name.to_owned();
    }
    (2..).map(|n| format!("{name} ({n})")).find(|n| !taken.contains(n)).unwrap_or_else(|| format!("{name} ({})", crate::util::new_id()))
}

/// The member of `repo` whose copy is `path` on `machine`.
fn member_at(hub: &Hub, repo: &str, machine: &str, path: &str) -> Option<u64> {
    hub.sync.view().repos.into_iter().find(|r| r.id == repo)?.copies.into_iter().find(|c| c.machine == machine && c.path == path).map(|c| c.member)
}

/// One machine's clone for the batch: the commit it checked out, and whether it cloned it now. A
/// folder there already that kiln made for this repository (a task tried before) is taken as it is.
async fn clone_on(hub: &Arc<Hub>, b: &Batch, id: &str, machine: &str, path: &str, source: &Commits) -> Result<(Option<String>, bool), String> {
    if let Some(f) = hub.places.found(machine, path)
        && f.kiln
        && f.origin.as_deref().and_then(super::places::key_of).as_deref() == Some(b.key.as_str())
    {
        return Ok((f.head, false));
    }
    let (bundle, hash, size, url) = match source {
        Commits::Folder => return Ok((None, false)),
        Commits::Bundle { hash, size } => (Some(b.id.clone()), Some(hash.clone()), *size, None),
        Commits::Url(u) => (None, None, 0, Some(u.clone())),
    };
    hub.tasks.set(hub, id, "cloning", format!("{machine} clones into {path}"));
    let order = CloneOrder {
        task: id.to_owned(),
        key: b.key.clone(),
        path: path.to_owned(),
        bundle,
        hash,
        size,
        url,
        branch: b.branch.clone(),
        origin: b.origin.clone(),
        all_branches: b.kind == Kind::Move,
    };
    hub.sync.send(machine, Msg::Clone(order));
    let news = wait_news(hub, id, machine, &[id.to_owned()]).await?;
    Ok((news.head, true))
}

/// Waits for what `machine` says of task (or batch) `id` to be over, each piece of news shown on
/// the tasks `ids`: done, or why it failed. A machine silent for [`QUIET`], or away for [`AWAY`],
/// has failed.
async fn wait_news(hub: &Arc<Hub>, id: &str, machine: &str, ids: &[String]) -> Result<TaskNews, String> {
    let start = Instant::now();
    let mut said: Option<TaskNews> = None;
    let mut away: Option<Instant> = None;
    loop {
        let listener = hub.tasks.changed.listen();
        if let Some((news, at)) = hub.tasks.news(id, machine) {
            if said.as_ref() != Some(&news) {
                said = Some(news.clone());
                if !news.over() {
                    for t in ids {
                        hub.tasks.set(hub, t, &news.state, news.detail.clone().unwrap_or_default());
                    }
                }
            }
            if news.state == "done" {
                return Ok(news);
            }
            if news.state == "failed" {
                return Err(news.error.unwrap_or_else(|| format!("{machine} failed")));
            }
            if at.elapsed() > QUIET {
                return Err(format!("{machine} stopped saying how it goes"));
            }
        } else if start.elapsed() > QUIET {
            return Err(format!("{machine} never started: is its runner up to date?"));
        }
        let online = hub.agents.lock().get(machine).is_some_and(|a| a.connected);
        if online {
            away = None;
        } else if away.get_or_insert_with(Instant::now).elapsed() > AWAY {
            return Err(format!("{machine} went offline"));
        }
        let _ = crate::rt::timeout(Duration::from_secs(2), listener).await;
    }
}

/// Waits for a synced copy to be in step, having looked at its folder and taken the group's files
/// if it was to; how it is doing shown on the tasks `ids`.
async fn wait_in_step(hub: &Arc<Hub>, repo: &str, member: u64, ids: &[String], what: &str) -> Result<(), String> {
    let start = Instant::now();
    let mut bad: Option<Instant> = None;
    let mut said = String::new();
    loop {
        let view = hub.sync.view();
        let r = view.repos.iter().find(|r| r.id == repo).ok_or_else(|| "the synced repo is gone".to_owned())?;
        let c = r.copies.iter().find(|c| c.member == member).ok_or_else(|| format!("{what} is no longer synced"))?;
        let looked = c.status.as_ref().is_some_and(|s| s.looked && !s.adopting);
        if c.state == "in step" && looked {
            return Ok(());
        }
        let detail = match (&c.status, c.state) {
            (Some(s), "error") => format!("{what}: {}", s.error.clone().unwrap_or_default()),
            (Some(s), _) if s.adopting => format!("{what} takes the files ({} to take, fetching {})", s.behind, s.waiting),
            (Some(s), "catching up" | "fetching") => format!("{what} catches up ({} to take, fetching {})", s.behind, s.waiting),
            (Some(s), "sending") => format!("{what} sends {} changes", s.unsent),
            (_, state) => format!("{what}: {state}"),
        };
        if detail != said {
            for t in ids {
                hub.tasks.set(hub, t, "catching up", detail.clone());
            }
            said = detail;
        }
        if c.state == "error" || c.state == "offline" {
            let since = bad.get_or_insert_with(Instant::now);
            if since.elapsed() > AWAY {
                return Err(if c.state == "offline" { format!("{what} is offline") } else { said });
            }
        } else {
            bad = None;
        }
        if start.elapsed() > CATCH_UP {
            return Err(format!("{what} didn't come in step in {} hours", CATCH_UP.as_secs() / 3600));
        }
        crate::rt::sleep(Duration::from_secs(1)).await;
    }
}

/// A bundle of kiln's mirror of a repository it builds (fetched first), kept as the batch's: its
/// SHA-256 and size.
async fn mirror_bundle(hub: &Arc<Hub>, repo: &str, batch: &str) -> anyhow::Result<(String, u64)> {
    let r = hub.store.repos().await?.into_iter().find(|r| r.id == repo).ok_or_else(|| anyhow::anyhow!("kiln doesn't build that repository any more"))?;
    let mirror = crate::gitops::sync_mirror(hub, &r).await?;
    let file = hub.tasks.bundle_file(batch).ok_or_else(|| anyhow::anyhow!("a batch with no name"))?;
    let (m, f) = (mirror.to_string_lossy().into_owned(), file.to_string_lossy().into_owned());
    crate::gitops::run_git(hub, &["-C", &m, "bundle", "create", &f, "--branches", "--tags", "HEAD"], Duration::from_secs(3600)).await?;
    crate::rt::blocking(move || -> anyhow::Result<(String, u64)> {
        let size = std::fs::metadata(&file)?.len();
        Ok((super::blobs::hash_file(&file)?, size))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_cut_short_by_a_restart_failed_and_old_ones_go() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let now = crate::util::now_unix();
        let task = |id: &str, state: &str, updated: i64| Task {
            id: id.into(),
            batch: "b".into(),
            kind: Kind::Clone,
            key: "github.com/a/b".into(),
            name: "a/b".into(),
            from: Source::Url { url: "https://github.com/a/b".into() },
            branch: None,
            machine: "m".into(),
            path: "/x".into(),
            delete: false,
            state: state.into(),
            detail: String::new(),
            error: None,
            notes: Vec::new(),
            created: updated,
            updated,
            synced: None,
            head: None,
            mode: super::super::Mode::Files,
        };
        let saved = vec![task("running", "cloning", now), task("done", "done", now), task("ancient", "done", now - KEEP_FOR - 10)];
        std::fs::write(dir.path().join("tasks.json"), serde_json::to_vec(&saved)?)?;
        std::fs::create_dir_all(dir.path().join("bundles"))?;
        std::fs::write(dir.path().join("bundles/old.bundle"), "x")?;
        let t = Tasks::open(dir.path());
        let list = t.list();
        assert_eq!(list.len(), 2, "a task over a week ago is gone");
        let cut = list.iter().find(|t| t.id == "running").map(|t| (t.state.as_str(), t.error.is_some()));
        assert_eq!(cut, Some(("failed", true)));
        assert!(!dir.path().join("bundles/old.bundle").exists(), "a restart's leftovers go");
        assert!(t.dismiss("done") && !t.dismiss("nope"));
        assert_eq!(t.bundle_file("../etc"), None);
        assert!(t.bundle_file("0199aa").is_some());
        Ok(())
    }

    /// Whether `ok` holds within ten seconds.
    async fn soon(ok: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while !ok() && start.elapsed() < Duration::from_secs(10) {
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        ok()
    }

    #[test]
    fn each_machine_s_clone_is_done_once_that_machine_says_so_however_slow_the_others() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let now = crate::util::now_unix();
            let from = Source::Url { url: "https://github.com/a/b".into() };
            let task = |id: &str, machine: &str| Task {
                id: id.into(),
                batch: "b1".into(),
                kind: Kind::Clone,
                key: "github.com/a/b".into(),
                name: "a/b".into(),
                from: from.clone(),
                branch: None,
                machine: machine.into(),
                path: "/src/b".into(),
                delete: false,
                state: "queued".into(),
                detail: String::new(),
                error: None,
                notes: Vec::new(),
                created: now,
                updated: now,
                synced: None,
                head: None,
                mode: crate::sync::Mode::Files,
            };
            hub.tasks.inner.lock().tasks.extend([task("quick", "laptop"), task("slow", "busy-box")]);
            let b = Batch {
                id: "b1".into(),
                kind: Kind::Clone,
                key: "github.com/a/b".into(),
                name: "a/b".into(),
                from: from.clone(),
                branch: None,
                tasks: vec![("quick".into(), "laptop".into(), "/src/b".into()), ("slow".into(), "busy-box".into(), "/src/b".into())],
                delete: false,
                origin: None,
                source_git: false,
                source_copy: None,
                synced: None,
                mode: crate::sync::Mode::Files,
            };
            let h = hub.clone();
            crate::rt::spawn(async move { drive(h, b).await });
            let state = |id: &str| hub.tasks.get(id).map(|t| t.state).unwrap_or_default();
            anyhow::ensure!(soon(|| state("quick") == "cloning" && state("slow") == "cloning").await, "both machines were told to clone");
            // The laptop is done; the busy box says nothing yet.
            hub.tasks.heard("laptop", TaskNews { head: Some("c0ffee".into()), ..TaskNews::new("quick", "done", "cloned main into /src/b") });
            anyhow::ensure!(soon(|| state("quick") == "done").await, "the laptop's clone shows done at once");
            assert_eq!(hub.tasks.get("quick").and_then(|t| t.head).as_deref(), Some("c0ffee"));
            assert_eq!(state("slow"), "cloning", "the busy box's clone is still under way");
            Ok(())
        })
    }

    #[test]
    fn asks_read_as_the_console_sends_them() -> anyhow::Result<()> {
        let a: Ask = serde_json::from_str(r#"{"kind":"move","key":"github.com/a/b","from":{"kind":"machine","machine":"mac","path":"/Users/a/b"},"to":[{"machine":"box","path":"~/src/b"}],"delete":true}"#)?;
        assert_eq!((a.kind, a.to.len(), a.delete), (Kind::Move, 1, true));
        assert_eq!(a.from, Some(Source::Machine { machine: "mac".into(), path: "/Users/a/b".into() }));
        let c: Ask = serde_json::from_str(r#"{"kind":"clone","key":"k","from":{"kind":"kiln","repo":"r1"},"branch":"main","to":[]}"#)?;
        assert_eq!((c.kind, c.from, c.branch.as_deref()), (Kind::Clone, Some(Source::Kiln { repo: "r1".into() }), Some("main")));
        Ok(())
    }
}
