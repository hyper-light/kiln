//! A runner's keeper of the copies of other runners' trees on its machine
//! (`~/.kiln/reach/<runner>/<tree>`, docs/reach.md §5.1): it notes when each is used and by which
//! agent ([`Using`], as the node's syncs and runs say), looks at them every 20 seconds with the room
//! left on their disk, lets go of what the rules ([`keep`]) don't keep when a trigger says, and tells
//! the server what it holds, what it let go and why. The rules come back in the server's answer and
//! are kept beside the copies, so they apply while the server is away. One runner process of a
//! machine keeps them at a time (it holds `.keep.lock` beside them); one that handed everything to
//! a newer build lets the newer one keep them ([`retire`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::keep::{self, Disk, Gone, Look, Report, Set, Step, TreeCopy};
use super::node::Handle;

/// What a run carries to say which agent asked for it (its session), for the copy it runs in.
pub const AGENT_ENV: &str = "KILN_REACH_AGENT";

/// How often the keeper looks.
const TICK: Duration = Duration::from_secs(20);
/// Copies let go that the keeper remembers, newest first.
const GONE_KEPT: usize = 50;
/// Agents noted for a copy, the latest last.
const AGENTS_KEPT: usize = 4;
/// A copy that changes is measured again at most this often.
const SIZE_EVERY_S: i64 = 600;
/// What measuring copies may take of one look (the first copy measured always is).
const SIZE_BUDGET: Duration = Duration::from_secs(5);
/// The keeper's file is written when it changes, and at least this often.
const SAVE_EVERY_S: i64 = 600;
/// How long the server has to answer.
const ASK_WAIT: Duration = Duration::from_secs(20);
/// Beside the copies: the keeper's file, its lock, and where copies are moved before they're
/// deleted (a deletion cut short is finished there).
const SAVED: &str = ".keep.json";
const LOCK: &str = ".keep.lock";
const ASIDE: &str = ".letting-go";
/// The folder `kiln run`'s named shells keep their state in, beside the copies: not a runner's.
const SHELLS: &str = "shells";

static RETIRING: AtomicBool = AtomicBool::new(false);

/// This runner process handed everything to a newer build of its machine: it keeps the copies no
/// more, and its lock goes, so the newer one keeps them.
pub fn retire() {
    RETIRING.store(true, Ordering::Relaxed);
}

/// What the node does with each copy: syncs and runs at it now, when it was last used, and the
/// agent that last ran there. The node notes uses ([`Using::busy`]); the keeper reads them and lets
/// a copy go only while none is under way ([`Using::take`]).
#[derive(Debug, Default)]
pub struct Using {
    held: Mutex<HashMap<(String, String), Use>>,
}

#[derive(Debug, Clone, Default)]
pub struct Use {
    /// Syncs and runs at it now.
    pub busy: u32,
    /// When one last started or ended (unix seconds).
    pub used: i64,
    pub agent: Option<String>,
}

/// A sync or a run at a copy: it's in use until this drops.
#[derive(Debug)]
pub struct Busy {
    using: Arc<Using>,
    key: (String, String),
}

impl Using {
    /// Runner `from`'s copy `key` is in use from now until the guard drops; `agent` asked for it.
    pub fn busy(self: &Arc<Self>, from: &str, key: &str, agent: Option<String>) -> Busy {
        let key = (from.to_owned(), key.to_owned());
        let mut held = self.held.lock();
        let u = held.entry(key.clone()).or_default();
        u.busy = u.busy.saturating_add(1);
        u.used = crate::util::now_unix();
        if agent.is_some() {
            u.agent = agent;
        }
        Busy { using: Arc::clone(self), key }
    }

    /// Every copy the node noted, as it stands now.
    pub(crate) fn snapshot(&self) -> HashMap<(String, String), Use> {
        self.held.lock().clone()
    }

    /// Move copy `root` aside to `aside`, unless a sync or a run is at it: under the lock
    /// [`Using::busy`] takes, so none starts on it halfway. One that starts once it's gone makes it
    /// again, whole.
    fn take(&self, from: &str, key: &str, root: &Path, aside: &Path) -> std::io::Result<bool> {
        let mut held = self.held.lock();
        let k = (from.to_owned(), key.to_owned());
        if held.get(&k).is_some_and(|u| u.busy > 0) {
            return Ok(false);
        }
        std::fs::rename(root, aside)?;
        held.remove(&k);
        Ok(true)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(u) = self.using.held.lock().get_mut(&self.key) {
            u.busy = u.busy.saturating_sub(1);
            u.used = crate::util::now_unix();
        }
    }
}

/// The agent a run says asked for it ([`AGENT_ENV`]), when it's a name kiln gives one.
pub fn agent_of(env: &[(String, String)]) -> Option<String> {
    env.iter()
        .rev()
        .find(|(k, _)| k == AGENT_ENV)
        .map(|(_, v)| v.trim().to_owned())
        .filter(|v| !v.is_empty() && v.len() <= 128 && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
}

/// Whether `name` is a copy's folder as `super::sync::mirror_key` names them: a folder's name and
/// ten hex digits of where it is.
pub fn is_key(name: &str) -> bool {
    let Some((base, hash)) = name.rsplit_once('-') else { return false };
    !name.starts_with('.')
        && hash.len() == 10
        && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && base.len() <= 48
        && base.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// What the keeper notes of a copy besides what its folder says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Record {
    created: i64,
    used: i64,
    agents: Vec<String>,
    repo: Option<String>,
    /// When its repository was last read from it.
    read: i64,
    bytes: Option<u64>,
    /// When it was last measured.
    sized: i64,
}

/// The keeper's file beside the copies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    /// Its last look (unix seconds).
    since: i64,
    set: Set,
    /// The last ask to let go now the server passed on, and the last carried out.
    asked: u64,
    done: u64,
    /// Each copy's record, by `<runner>/<folder>`.
    records: BTreeMap<String, Record>,
    gone: Vec<Gone>,
}

struct Keeper {
    base: PathBuf,
    using: Arc<Using>,
    handle: Handle,
    lock: Option<std::fs::File>,
    saved: Saved,
    /// As last written, and when.
    written: Option<Saved>,
    written_at: i64,
    error: Option<String>,
}

/// Keep the copies under `base` (`~/.kiln/reach`), as the node at `handle` notes their uses in
/// `using`.
pub fn start(handle: Handle, using: Arc<Using>, base: PathBuf) {
    crate::rt::spawn(async move {
        let mut k = Keeper { base, using, handle, lock: None, saved: Saved::default(), written: None, written_at: 0, error: None };
        while !RETIRING.load(Ordering::Relaxed) {
            let again = std::panic::AssertUnwindSafe(k.look()).catch_unwind().await.unwrap_or_else(|_| {
                crate::metrics::global().panics_caught.inc();
                tracing::error!(target: "kiln.reach", "the keeper of copies panicked; carrying on");
                false
            });
            if !again {
                crate::rt::sleep(TICK).await;
            }
        }
        tracing::info!(target: "kiln.reach", held = k.lock.is_some(), "this runner process retires: the newer one keeps the copies");
    });
}

impl Keeper {
    /// One look: what's here, what goes, what the server says. True when it should look again at
    /// once (the server passed on an ask to let go now).
    async fn look(&mut self) -> bool {
        let now = crate::util::now_unix();
        if !self.base.is_dir() {
            // No copies here yet: nothing to keep, but the server hears so, and has its rules ready.
            self.saved.done = self.saved.asked;
            return self.report(now, disk_of(&self.base), Vec::new()).await;
        }
        if !self.hold().await {
            return false;
        }
        let (base, records, using) = (self.base.clone(), std::mem::take(&mut self.saved.records), self.using.snapshot());
        let found = crate::rt::blocking(move || inventory(&base, records, &using, now)).await;
        let mut copies = match found {
            Ok(Ok((copies, records))) => {
                self.saved.records = records;
                copies
            }
            Ok(Err(e)) => return self.failed(now, format!("its copies couldn't be looked at: {e}")).await,
            Err(e) => return self.failed(now, format!("its copies couldn't be looked at: {e:#}")).await,
        };
        self.measure(&mut copies, now).await;
        let disk = disk_of(&self.base);
        let asked = self.saved.asked > self.saved.done;
        // The first look of all catches up on no schedule.
        let since = if self.saved.since == 0 { now } else { self.saved.since };
        self.error = None;
        for step in keep::plan(&self.saved.set, &Look { now, since, disk, asked, copies: &copies }) {
            let copy = copies.iter().position(|c| c.from == step.from && c.key == step.key);
            if let Some(g) = self.let_go(&step, copy.and_then(|i| copies.get(i)), now).await {
                if let Some(i) = copy {
                    copies.remove(i);
                }
                self.saved.gone.insert(0, g);
                self.saved.gone.truncate(GONE_KEPT);
            }
        }
        self.saved.since = now;
        if asked {
            self.saved.done = self.saved.asked;
        }
        measured(&copies);
        let disk = disk_of(&self.base);
        let again = self.report(now, disk, copies).await;
        self.save(now).await;
        again
    }

    async fn failed(&mut self, now: i64, why: String) -> bool {
        if self.error.as_deref() != Some(why.as_str()) {
            tracing::warn!(target: "kiln.reach", event = "keep_failed", error = %why, "the keeper of copies couldn't look at them");
        }
        self.error = Some(why);
        self.report(now, disk_of(&self.base), Vec::new()).await
    }

    /// Whether this process keeps the copies: the one holding `.keep.lock` beside them. Taking it,
    /// it reads what was kept before and deletes what a let-go cut short left aside.
    async fn hold(&mut self) -> bool {
        if self.lock.is_some() {
            return true;
        }
        let base = self.base.clone();
        let got = crate::rt::blocking(move || -> std::io::Result<Option<(std::fs::File, Saved)>> {
            let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(base.join(LOCK))?;
            if f.try_lock().is_err() {
                return Ok(None);
            }
            let saved = std::fs::read(base.join(SAVED)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            for e in std::fs::read_dir(base.join(ASIDE)).into_iter().flatten().flatten() {
                let _ = remove_all(&e.path());
            }
            Ok(Some((f, saved)))
        })
        .await;
        match got {
            Ok(Ok(Some((f, saved)))) => {
                tracing::info!(target: "kiln.reach", base = %self.base.display(), "this runner process keeps the copies of other runners' trees");
                self.lock = Some(f);
                self.written = Some(saved.clone());
                // What the rules said, what was asked, and the records: as kept, with what this
                // process heard from the server meanwhile, if it did.
                let heard = (self.saved.set != Set::default()).then(|| (self.saved.set.clone(), self.saved.asked));
                self.saved = saved;
                if let Some((set, asked)) = heard {
                    self.saved.set = set;
                    self.saved.asked = self.saved.asked.max(asked);
                }
                true
            }
            Ok(Ok(None)) => false,
            Ok(Err(e)) => {
                self.error = Some(format!("its copies' lock couldn't be taken: {e}"));
                false
            }
            Err(_) => false,
        }
    }

    /// Measure copies that changed since they last were, and those never measured, for a while.
    async fn measure(&mut self, copies: &mut [TreeCopy], now: i64) {
        let mut due: Vec<(String, PathBuf, bool)> = copies
            .iter()
            .filter(|c| !c.busy)
            .filter_map(|c| {
                let r = self.saved.records.get(&format!("{}/{}", c.from, c.key))?;
                let stale = r.bytes.is_none() || (r.sized < r.used && now.saturating_sub(r.sized) >= SIZE_EVERY_S);
                stale.then(|| (format!("{}/{}", c.from, c.key), self.base.join(&c.from).join(&c.key), r.bytes.is_none()))
            })
            .collect();
        if due.is_empty() {
            return;
        }
        // Never measured first.
        due.sort_by_key(|(_, _, never)| !*never);
        let sizes = crate::rt::blocking(move || {
            let started = Instant::now();
            let mut out = Vec::new();
            for (id, root, _) in due {
                if !out.is_empty() && started.elapsed() > SIZE_BUDGET {
                    break;
                }
                out.push((id, size_of(&root)));
            }
            out
        })
        .await
        .unwrap_or_default();
        for (id, bytes) in sizes {
            if let Some(r) = self.saved.records.get_mut(&id) {
                r.bytes = Some(bytes);
                r.sized = now;
            }
            if let Some(c) = copies.iter_mut().find(|c| format!("{}/{}", c.from, c.key) == id) {
                c.bytes = Some(bytes);
            }
        }
    }

    /// Let `copy` go as `step` says (only while the room is short, for room): moved aside at once,
    /// unless it's in use now, then deleted. What went, and why; none when it stays.
    async fn let_go(&mut self, step: &Step, copy: Option<&TreeCopy>, now: i64) -> Option<Gone> {
        if let Some(wanted) = step.room
            && disk_of(&self.base).is_some_and(|d| d.free >= wanted)
        {
            return None;
        }
        let (root, folder) = (self.base.join(&step.from).join(&step.key), self.base.join(&step.from));
        let aside = self.base.join(ASIDE).join(format!("{}--{}--{}", step.from, step.key, crate::util::now_ms()));
        let (using, from, key) = (Arc::clone(&self.using), step.from.clone(), step.key.clone());
        let moved = crate::rt::blocking(move || -> std::io::Result<Option<std::io::Result<()>>> {
            if let Some(dir) = aside.parent() {
                std::fs::create_dir_all(dir)?;
            }
            if !using.take(&from, &key, &root, &aside)? {
                return Ok(None);
            }
            // Deleted where it went: a sync of the tree from now on makes a copy afresh.
            let gone = remove_all(&aside);
            // Its runner's folder goes with its last copy.
            let _ = std::fs::remove_dir(&folder);
            Ok(Some(gone))
        })
        .await;
        // In use now, it stays; one that couldn't be moved aside stays too, and the next look tries
        // again. Moved aside, it's no copy any more, whatever deleting it left (the next look
        // finishes that).
        let error = match moved {
            Ok(Ok(None)) => return None,
            Ok(Ok(Some(removed))) => removed.err().map(|e| format!("moved aside, but not all deleted yet: {e}")),
            Ok(Err(e)) => {
                self.error = Some(format!("{}/{} couldn't be let go: {e}", step.from, step.key));
                return None;
            }
            Err(e) => {
                self.error = Some(format!("{}/{} couldn't be let go: {e:#}", step.from, step.key));
                return None;
            }
        };
        self.saved.records.remove(&format!("{}/{}", step.from, step.key));
        let g = Gone {
            from: step.from.clone(),
            key: step.key.clone(),
            repo: copy.and_then(|c| c.repo.clone()),
            agent: copy.and_then(|c| c.agent().map(str::to_owned)),
            bytes: copy.and_then(|c| c.bytes),
            at: now,
            trigger: step.trigger,
            why: step.why.clone(),
            error,
        };
        match &g.error {
            None => tracing::info!(target: "kiln.reach", event = "let_go", from = %g.from, copy = %g.key, trigger = g.trigger.as_str(), bytes = g.bytes.unwrap_or(0), why = %g.why, "let go of a copy of another runner's tree"),
            Some(e) => tracing::warn!(target: "kiln.reach", event = "let_go", from = %g.from, copy = %g.key, trigger = g.trigger.as_str(), error = %e, why = %g.why, "a copy of another runner's tree couldn't all be let go"),
        }
        let m = crate::metrics::sync();
        m.reach_let_go.inc(&[g.trigger.as_str()]);
        m.reach_let_go_bytes.add(&[g.trigger.as_str()], g.bytes.unwrap_or(0));
        Some(g)
    }

    /// Tell the server what's here and what went; take its rules and its ask. True when it passed
    /// on an ask to let go now that isn't carried out yet.
    async fn report(&mut self, now: i64, disk: Option<Disk>, copies: Vec<TreeCopy>) -> bool {
        let report = Report { at: now, base: self.base.to_string_lossy().into_owned(), disk, copies, gone: self.saved.gone.clone(), done: self.saved.done, error: self.error.clone() };
        let ask = serde_json::json!({ "op": "keep", "report": report }).to_string();
        let Ok(Ok(answer)) = crate::rt::timeout(ASK_WAIT, self.handle.query(ask)).await else { return false };
        // A server from before this doesn't know the question: the rules kept go on applying.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&answer) else { return false };
        if v.get("error").is_some() {
            return false;
        }
        let Ok(a) = serde_json::from_value::<keep::Answer>(v) else { return false };
        self.saved.set = a.set;
        self.saved.asked = a.asked;
        self.saved.asked > self.saved.done
    }

    /// Write the keeper's file beside the copies when it changed (its last look aside), and now and
    /// then for that.
    async fn save(&mut self, now: i64) {
        if self.lock.is_none() {
            return;
        }
        let same = self.written.as_ref().is_some_and(|w| Saved { since: 0, ..w.clone() } == Saved { since: 0, ..self.saved.clone() });
        if same && now.saturating_sub(self.written_at) < SAVE_EVERY_S {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(&self.saved) else { return };
        let path = self.base.join(SAVED);
        match crate::rt::blocking(move || crate::sync::group::save_atomic(&path, &bytes)).await {
            Ok(Ok(())) => {
                self.written = Some(self.saved.clone());
                self.written_at = now;
            }
            Ok(Err(e)) => tracing::warn!(target: "kiln.reach", error = %e, "the keeper of copies couldn't write its file"),
            Err(e) => tracing::warn!(target: "kiln.reach", error = %format!("{e:#}"), "the keeper of copies couldn't write its file"),
        }
    }
}

/// The copies under `base`, each with its record (one seen first gets one from its folder's
/// times), what the node noted of it since, and its repository read again when it was used since.
fn inventory(base: &Path, mut records: BTreeMap<String, Record>, using: &HashMap<(String, String), Use>, now: i64) -> std::io::Result<(Vec<TreeCopy>, BTreeMap<String, Record>)> {
    let mut copies = Vec::new();
    let mut seen = BTreeMap::new();
    // What a let-go couldn't all delete, finished.
    for e in std::fs::read_dir(base.join(ASIDE)).into_iter().flatten().flatten() {
        let _ = remove_all(&e.path());
    }
    for runner in std::fs::read_dir(base)?.flatten() {
        let from = runner.file_name().to_string_lossy().into_owned();
        if from.starts_with('.') || from == SHELLS || !runner.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(folders) = std::fs::read_dir(runner.path()) else { continue };
        for folder in folders.flatten() {
            let key = folder.file_name().to_string_lossy().into_owned();
            if !is_key(&key) || !folder.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let root = folder.path();
            let id = format!("{from}/{key}");
            let mut r = records.remove(&id).unwrap_or_else(|| {
                let t = touched(&root).unwrap_or(now);
                Record { created: t, used: t, ..Record::default() }
            });
            let u = using.get(&(from.clone(), key.clone()));
            if let Some(u) = u {
                r.used = r.used.max(u.used);
                if let Some(a) = u.agent.as_ref().filter(|a| r.agents.last() != Some(*a)) {
                    r.agents.retain(|x| x != a);
                    r.agents.push(a.clone());
                    let extra = r.agents.len().saturating_sub(AGENTS_KEPT);
                    r.agents.drain(..extra);
                }
            }
            if r.read == 0 || r.read < r.used {
                r.repo = repo_of(&root);
                r.read = now;
            }
            copies.push(TreeCopy {
                from: from.clone(),
                key: key.clone(),
                repo: r.repo.clone(),
                agents: r.agents.clone(),
                created: r.created,
                used: r.used,
                bytes: r.bytes,
                busy: u.is_some_and(|u| u.busy > 0),
            });
            seen.insert(id, r);
        }
    }
    Ok((copies, seen))
}

/// The copies' gauges: how many each runner's tree has here, and their bytes.
fn measured(copies: &[TreeCopy]) {
    let mut per: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for c in copies {
        let e = per.entry(c.from.as_str()).or_default();
        e.0 += 1;
        e.1 = e.1.saturating_add(i64::try_from(c.bytes.unwrap_or(0)).unwrap_or(i64::MAX));
    }
    let m = crate::metrics::sync();
    m.reach_copies.replace(per.iter().map(|(f, (n, _))| (vec![(*f).to_owned()], *n)));
    m.reach_copies_bytes.replace(per.iter().map(|(f, (_, b))| (vec![(*f).to_owned()], *b)));
}

/// When a copy kiln has no record of was last touched: its folder's, its git folder's or its
/// index's time, whichever is latest (a copy from before records were kept).
fn touched(root: &Path) -> Option<i64> {
    [root.to_path_buf(), root.join(".git"), root.join(".git").join("index")]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .max()
}

/// The repository a copy is of: its `origin` (`crate::sync::places::key_of`), read from its git
/// config.
fn repo_of(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(".git").join("config")).ok()?;
    let mut origin = false;
    for line in text.lines().map(str::trim) {
        if let Some(section) = line.strip_prefix('[') {
            origin = section.trim_end_matches(']').split_whitespace().eq(["remote", "\"origin\""]);
            continue;
        }
        if origin
            && let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case("url")
        {
            return crate::sync::places::key_of(v.trim());
        }
    }
    None
}

/// What a copy takes on its disk: its files' blocks (their bytes where there are none).
fn size_of(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            // Not through a symlink.
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                dirs.push(e.path());
            } else {
                total = total.saturating_add(on_disk(&md));
            }
        }
    }
    total
}

fn on_disk(md: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        md.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        md.len()
    }
}

/// The disk `base` is on (or would be, before the first copy).
fn disk_of(base: &Path) -> Option<Disk> {
    base.ancestors().find(|p| p.exists()).and_then(crate::os::disk_space).map(|(total, free)| Disk { total, free })
}

/// Delete `path` and all under it. A read-only file or folder stops a removal on some systems
/// (git's objects on Windows, a tool's read-only cache folder): made writable, it's tried again.
fn remove_all(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(first) => {
            writable(path);
            match std::fs::remove_dir_all(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(first),
                _ => Ok(()),
            }
        }
    }
}

fn writable(path: &Path) {
    let mut todo = vec![path.to_path_buf()];
    while let Some(p) = todo.pop() {
        let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
        if md.file_type().is_symlink() {
            continue;
        }
        let mut perm = md.permissions();
        if perm.readonly() {
            // Only on what's about to be deleted.
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = std::fs::set_permissions(&p, perm);
        }
        if md.is_dir() {
            todo.extend(std::fs::read_dir(&p).into_iter().flatten().flatten().map(|e| e.path()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_folders_are_told_apart_from_the_rest() {
        assert!(is_key("kiln-0123456789"));
        assert!(is_key("data-layer-gate-9a49a6e6d1"));
        assert!(is_key("x.y_z-abcdef0123"));
        for not in ["kiln", "kiln-012345678", "kiln-0123456789a", "kiln-ABCDEF0123", ".hidden-0123456789", "a/b-0123456789", "-0123456789x"] {
            assert!(!is_key(not), "{not}");
        }
        assert!(is_key(&super::super::sync::mirror_key("mac", Path::new("/Users/a/Projects/kiln"))));
        assert!(is_key(&super::super::sync::mirror_key("mac", Path::new("/x/.hidden"))));
    }

    #[test]
    fn a_run_names_its_agent_when_it_is_a_name_kiln_gives() {
        let env = |v: &str| vec![("PATH".to_owned(), "/bin".to_owned()), (AGENT_ENV.to_owned(), v.to_owned())];
        assert_eq!(agent_of(&env("01a17596ec827723932509547be17da9")).as_deref(), Some("01a17596ec827723932509547be17da9"));
        assert_eq!(agent_of(&env("a b")), None);
        assert_eq!(agent_of(&env("")), None);
        assert_eq!(agent_of(&[]), None);
    }

    #[test]
    fn a_copy_in_use_is_not_taken_and_one_done_with_is() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("mac").join("kiln-0123456789");
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n")?;
        let using = Arc::new(Using::default());
        let aside = dir.path().join(ASIDE).join("x");
        std::fs::create_dir_all(dir.path().join(ASIDE))?;
        let busy = using.busy("mac", "kiln-0123456789", Some("agent-1".into()));
        assert!(!using.take("mac", "kiln-0123456789", &root, &aside)?);
        assert!(root.is_dir());
        let seen = using.snapshot();
        assert!(seen.get(&("mac".into(), "kiln-0123456789".into())).is_some_and(|u| u.busy == 1 && u.agent.as_deref() == Some("agent-1")));
        drop(busy);
        assert!(using.take("mac", "kiln-0123456789", &root, &aside)?);
        assert!(!root.exists() && aside.join("src/a.rs").is_file());
        remove_all(&aside)?;
        assert!(!aside.exists());
        Ok(())
    }

    #[test]
    fn the_keeper_lets_go_what_its_rules_dont_keep_when_its_schedule_comes_and_says_why() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
            let base = dir.path().to_path_buf();
            let now = crate::util::now_unix();
            for key in ["old-0123456789", "new-0123456789", "busy-0123456789"] {
                std::fs::create_dir_all(base.join("mac").join(key).join("src"))?;
                std::fs::write(base.join("mac").join(key).join("src/a.rs"), "fn a() {}\n")?;
            }
            // What was kept before: the rules (the newest two stay; a schedule every minute), the
            // last look a while ago, and when each copy was last used (the one in use is used now,
            // so it's the newest).
            let record = |days: i64| Record { created: now - days * 86_400, used: now - days * 86_400, read: now, ..Record::default() };
            let saved = Saved {
                since: now - 120,
                set: Set { system: keep::Rules { newest: Some(2), schedule: Some("* * * * *".into()), ..keep::Rules::default() }, ..Set::default() },
                records: BTreeMap::from([("mac/old-0123456789".into(), record(30)), ("mac/new-0123456789".into(), record(1)), ("mac/busy-0123456789".into(), record(40))]),
                ..Saved::default()
            };
            std::fs::write(base.join(SAVED), serde_json::to_vec(&saved)?)?;
            let using = Arc::new(Using::default());
            let _busy = using.busy("mac", "busy-0123456789", None);
            // A node that never reached the server: the rules kept apply.
            let (handle, _out) = super::super::node::start(super::super::node::Config {
                name: "linux".into(),
                dir: state.path().join("reach"),
                accept: false,
                mirrors: base.clone(),
                adopt: None,
                using: Arc::clone(&using),
                objects: None,
            })?;
            let mut k = Keeper { base: base.clone(), using, handle, lock: None, saved: Saved::default(), written: None, written_at: 0, error: None };
            assert!(!k.look().await);
            assert!(!base.join("mac/old-0123456789").exists(), "not kept: let go");
            assert!(base.join("mac/new-0123456789/src/a.rs").is_file(), "the newest stays");
            assert!(base.join("mac/busy-0123456789/src/a.rs").is_file(), "in use, it stays whatever the rules say");
            let gone = k.saved.gone.first().cloned().unwrap_or_default();
            assert_eq!((gone.key.as_str(), gone.trigger, gone.error.as_deref()), ("old-0123456789", keep::Trigger::Schedule, None));
            assert!(gone.why.contains("its schedule came round") && gone.why.contains("not one of the newest 2"), "{}", gone.why);
            assert!(std::fs::read_dir(base.join(ASIDE)).map(|d| d.count()).unwrap_or(0) == 0, "nothing left aside");
            // What it did is kept beside the copies, for the next look and the next process.
            let written: Saved = serde_json::from_slice(&std::fs::read(base.join(SAVED))?)?;
            assert!(written.since >= now && !written.records.contains_key("mac/old-0123456789") && written.gone.len() == 1);
            // Its schedule doesn't come round again within the same minute: nothing more goes.
            assert!(!k.look().await);
            assert_eq!(k.saved.gone.len(), 1);
            Ok(())
        })
    }

    #[test]
    fn the_inventory_finds_copies_their_repository_and_who_used_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let base = dir.path();
        let root = base.join("mac").join("kiln-0123456789");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:hyper-light/kiln.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n")?;
        std::fs::write(root.join("big.bin"), vec![1u8; 64 * 1024])?;
        // Not copies: the named shells' folder, the keeper's own, a folder of another shape.
        std::fs::create_dir_all(base.join(SHELLS).join("build-0123456789"))?;
        std::fs::create_dir_all(base.join(ASIDE).join("old"))?;
        std::fs::create_dir_all(base.join("mac").join("not-a-copy"))?;
        let using = Arc::new(Using::default());
        drop(using.busy("mac", "kiln-0123456789", Some("agent-1".into())));
        let _busy = using.busy("mac", "kiln-0123456789", Some("agent-2".into()));
        let (copies, records) = inventory(base, BTreeMap::new(), &using.snapshot(), crate::util::now_unix())?;
        assert_eq!(copies.len(), 1, "{copies:?}");
        let c = copies.first().cloned().unwrap_or_default();
        assert_eq!((c.from.as_str(), c.key.as_str(), c.repo.as_deref()), ("mac", "kiln-0123456789", Some("github.com/hyper-light/kiln")));
        assert_eq!(c.agents, ["agent-2"]);
        assert!(c.busy && c.used > 0);
        assert!(records.contains_key("mac/kiln-0123456789"));
        assert!(size_of(&root) >= 64 * 1024);
        Ok(())
    }
}
