//! A runner's keeper of the copies of other runners' trees on its machine
//! (`~/.kiln/reach/<runner>/<tree>`, docs/reach.md §5.1): it notes when each is used and by which
//! agent ([`Using`], as the node's syncs and runs say), looks at them with the room left on their
//! disk, lets go of what the rules ([`keep`]) don't keep when a trigger says, and tells the server
//! what it holds, what it let go and why. The rules come back in the server's answer and are kept
//! beside the copies, so they apply while the server is away. One runner process of a machine keeps
//! them at a time (it holds `.keep.lock` beside them); one that handed everything to a newer build
//! lets the newer one keep them ([`retire`]).
//!
//! It looks on what changes what it decides, never on a clock (goal 9): a copy's use starting or
//! ending (a sync or a run at it: what kiln writes on the copies' disk), the connection to the
//! server coming up, the server's word that this runner's rules or its ask changed ([`told`],
//! `ServerMsg::Copies`), `.keep.lock` let go by the process that held it (a thread waits on it),
//! and its retirement; and at one due time, when its rules' schedule next comes round
//! ([`keep::due`]). A copy is measured once after each use of it ends, and the keeper's file is
//! written at each look that changed it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::keep::{self, Disk, Gone, Look, Report, Set, Step, TreeCopy};
use super::node::Handle;

/// What a run carries to say which agent asked for it (its session), for the copy it runs in.
pub const AGENT_ENV: &str = "KILN_REACH_AGENT";

/// Copies let go that the keeper remembers, newest first.
const GONE_KEPT: usize = 50;
/// Agents noted for a copy, the latest last.
const AGENTS_KEPT: usize = 4;
/// What measuring copies may take of one look (the first copy measured always is); what's left is
/// measured by the look after, at once.
const SIZE_BUDGET: Duration = Duration::from_secs(5);
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

/// What this process's keeper hears through ([`start`]'s).
static KEEPING: OnceLock<Arc<Using>> = OnceLock::new();

/// This runner process handed everything to a newer build of its machine: it keeps the copies no
/// more, and its lock goes, so the newer one keeps them.
pub fn retire() {
    RETIRING.store(true, Ordering::Relaxed);
    if let Some(u) = KEEPING.get() {
        u.inbox.wake.notify();
    }
}

/// The server's word on this runner's rules and its last ask to let go now
/// (`ServerMsg::Copies`, JSON [`keep::Answer`], as a report's answer carries them): taken at the
/// keeper's next look, which comes at once. One that can't be read is let go: the next report's
/// answer says the same.
pub fn told(json: &str) {
    let Ok(a) = serde_json::from_str::<keep::Answer>(json) else {
        tracing::debug!(target: "kiln.reach", "the server's word on this runner's copies doesn't read: its next answer says it");
        return;
    };
    if let Some(u) = KEEPING.get() {
        u.told(a);
    }
}

/// What wakes the keeper besides its schedule (goal 9): a copy's use starting or ending, the
/// connection to the server up, the server's word, the lock let go, retirement. Each notifies once;
/// what came meanwhile is taken at the next look.
#[derive(Default)]
struct Inbox {
    wake: crate::rt::Notify,
    /// The server's latest word ([`told`]), not yet taken.
    told: Mutex<Option<keep::Answer>>,
    /// `.keep.lock`, taken by the thread that waited for it, for the keeper's next look.
    lock: Mutex<Option<std::fs::File>>,
    /// A thread waits for the lock now.
    waiting: AtomicBool,
    /// The looks the keeper made, whatever woke it (the tests count an idle one's).
    looks: AtomicU64,
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox").field("looks", &self.looks.load(Ordering::Relaxed)).finish_non_exhaustive()
    }
}

/// What the node does with each copy: syncs and runs at it now, when it was last used, and the
/// agent that last ran there. The node notes uses ([`Using::busy`]); the keeper reads them and lets
/// a copy go only while none is under way ([`Using::take`]).
#[derive(Debug, Default)]
pub struct Using {
    held: Mutex<HashMap<(String, String), Use>>,
    /// What the keeper of these copies hears: every use starting or ending notifies it.
    inbox: Inbox,
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
        drop(held);
        self.inbox.wake.notify();
        Busy { using: Arc::clone(self), key }
    }

    /// The connection to the server came up (the node's word): the keeper tells it what's here, and
    /// hears its rules.
    pub fn connected(&self) {
        self.inbox.wake.notify();
    }

    /// The server's word on this runner's rules and ask ([`told`]), for the keeper's next look.
    fn told(&self, a: keep::Answer) {
        *self.inbox.told.lock() = Some(a);
        self.inbox.wake.notify();
    }

    /// The looks its keeper made so far.
    #[cfg(test)]
    fn looks(&self) -> u64 {
        self.inbox.looks.load(Ordering::Relaxed)
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
        // A use ended: the copy may have grown, and its disk's room with it (goal 9).
        self.using.inbox.wake.notify();
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
    /// As last written.
    written: Option<Saved>,
    error: Option<String>,
    /// When it next looks on its own (unix seconds), as its last look found it: its schedule's
    /// next time ([`keep::due`]), or a held copy's ([`keep::room_due`]).
    next: Option<i64>,
}

/// Keep the copies under `base` (`~/.kiln/reach`), as the node at `handle` notes their uses in
/// `using`.
pub fn start(handle: Handle, using: Arc<Using>, base: PathBuf) {
    let _ = KEEPING.set(Arc::clone(&using));
    crate::rt::spawn(Keeper::new(handle, using, base).run());
}

impl Keeper {
    fn new(handle: Handle, using: Arc<Using>, base: PathBuf) -> Keeper {
        Keeper { base, using, handle, lock: None, saved: Saved::default(), written: None, error: None, next: None }
    }

    /// Looks, then waits for what changes what it decides ([`Inbox`]) or for its schedule's next
    /// time, and looks again; until this process retires.
    async fn run(mut self) {
        while !RETIRING.load(Ordering::Relaxed) {
            self.using.inbox.looks.fetch_add(1, Ordering::Relaxed);
            let again = std::panic::AssertUnwindSafe(self.look()).catch_unwind().await.unwrap_or_else(|_| {
                crate::metrics::global().panics_caught.inc();
                tracing::error!(target: "kiln.reach", "the keeper of copies panicked; carrying on");
                false
            });
            if again || RETIRING.load(Ordering::Relaxed) {
                continue;
            }
            let due = self.due();
            futures::select_biased! {
                () = self.using.inbox.wake.notified().fuse() => {}
                () = crate::rt::due(due).fuse() => {}
            }
        }
        tracing::info!(target: "kiln.reach", held = self.lock.is_some(), "this runner process retires: the newer one keeps the copies");
    }

    /// Its one due time as its last look found it (its schedule's next, or, while the room is short,
    /// a held copy's), on this process's clock: from its first millisecond, so the look that wakes
    /// finds it come.
    fn due(&self) -> Option<Instant> {
        let at = self.next?;
        let ms = at.saturating_mul(1000).saturating_sub(crate::util::now_ms());
        Some(kiln_seams::clock::later(Instant::now(), Duration::from_millis(u64::try_from(ms).unwrap_or(0))))
    }

    /// One look: what's here, what goes, what the server says. True when it should look again at
    /// once (the server passed on an ask to let go now, or copies wait to be measured).
    async fn look(&mut self) -> bool {
        let now = crate::util::now_unix();
        self.next = None;
        // The server's word since the last look: as a report's answer would say it.
        if let Some(a) = self.using.inbox.told.lock().take() {
            self.saved.set = a.set;
            self.saved.asked = a.asked;
        }
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
        let unmeasured = self.measure(&mut copies, now).await;
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
        let kept = copies.clone();
        let again = self.report(now, disk, copies).await;
        // Its one due time: its schedule's next, or, while the room is short, when a copy held back
        // now could go.
        let room = keep::room_due(&self.saved.set, &Look { now, since: now, disk, asked: false, copies: &kept });
        self.next = [keep::due(&self.saved.set, &kept, disk, now), room].into_iter().flatten().min();
        self.save().await;
        again || unmeasured
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
        let waited = self.using.inbox.lock.lock().take();
        let got = crate::rt::blocking(move || -> std::io::Result<Option<(std::fs::File, Saved)>> {
            let f = match waited {
                Some(f) => f,
                None => {
                    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(base.join(LOCK))?;
                    if f.try_lock().is_err() {
                        return Ok(None);
                    }
                    f
                }
            };
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
            Ok(Ok(None)) => {
                self.wait_for_lock();
                false
            }
            Ok(Err(e)) => {
                self.error = Some(format!("its copies' lock couldn't be taken: {e}"));
                false
            }
            Err(_) => false,
        }
    }

    /// Another process of this machine keeps the copies: a thread waits for its lock, which that
    /// process's exit or retirement lets go, and wakes the keeper with it (goal 9: never asked again
    /// on a clock). Taken once this process retired, it's let go at once, for the newer one.
    fn wait_for_lock(&self) {
        if self.using.inbox.waiting.swap(true, Ordering::AcqRel) {
            return;
        }
        let (path, using) = (self.base.join(LOCK), Arc::clone(&self.using));
        let spawned = std::thread::Builder::new().name("kiln-keep-lock".into()).spawn(move || {
            let got = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).and_then(|f| f.lock().map(|()| f));
            using.inbox.waiting.store(false, Ordering::Release);
            match got {
                Ok(f) if !RETIRING.load(Ordering::Relaxed) => {
                    *using.inbox.lock.lock() = Some(f);
                    using.inbox.wake.notify();
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(target: "kiln.reach", error = %e, "the copies' lock couldn't be waited for: the keeper tries it again at its next look"),
            }
        });
        if let Err(e) = spawned {
            self.using.inbox.waiting.store(false, Ordering::Release);
            tracing::warn!(target: "kiln.reach", error = %e, "the copies' lock can't be waited for: the keeper tries it again at its next look");
        }
    }

    /// Measure copies never measured, and each once after a use of it ended (none under way): a use
    /// is what changes one. True when some wait for the next look, past [`SIZE_BUDGET`].
    async fn measure(&mut self, copies: &mut [TreeCopy], now: i64) -> bool {
        let mut due: Vec<(String, PathBuf, bool)> = copies
            .iter()
            .filter(|c| !c.busy)
            .filter_map(|c| {
                let r = self.saved.records.get(&format!("{}/{}", c.from, c.key))?;
                let stale = r.bytes.is_none() || r.sized < r.used;
                stale.then(|| (format!("{}/{}", c.from, c.key), self.base.join(&c.from).join(&c.key), r.bytes.is_none()))
            })
            .collect();
        if due.is_empty() {
            return false;
        }
        let wanted = due.len();
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
        let left = sizes.len() < wanted && !sizes.is_empty();
        for (id, bytes) in sizes {
            if let Some(r) = self.saved.records.get_mut(&id) {
                r.bytes = Some(bytes);
                r.sized = now;
            }
            if let Some(c) = copies.iter_mut().find(|c| format!("{}/{}", c.from, c.key) == id) {
                c.bytes = Some(bytes);
            }
        }
        left
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
        // Rules it didn't have (they changed while it was away, or its word was lost): applied by
        // the look after, at once.
        let changed = a.set != self.saved.set;
        self.saved.set = a.set;
        self.saved.asked = a.asked;
        changed || self.saved.asked > self.saved.done
    }

    /// Write the keeper's file beside the copies when the look changed it (its time included: the
    /// look a schedule is caught up from after a restart).
    async fn save(&mut self) {
        if self.lock.is_none() || self.written.as_ref() == Some(&self.saved) {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(&self.saved) else { return };
        let path = self.base.join(SAVED);
        match crate::rt::blocking(move || crate::sync::group::save_atomic(&path, &bytes)).await {
            Ok(Ok(())) => {
                self.written = Some(self.saved.clone());
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

    /// A node that never reaches a server, noting uses in `using`: the rules kept apply.
    fn node(state: &Path, base: &Path, using: &Arc<Using>) -> anyhow::Result<Handle> {
        let (handle, _out) = super::super::node::start(super::super::node::Config {
            name: "linux".into(),
            dir: state.join("reach"),
            accept: false,
            mirrors: base.to_path_buf(),
            adopt: None,
            using: Arc::clone(using),
            objects: None,
            sense: false,
            on_battery: kiln_ci::pace::power::OnBattery::default(),
            version: super::super::wire::VERSION,
            root: Vec::new(),
        })?;
        Ok(handle)
    }

    /// A copy `key` of runner `mac`'s tree under `base`.
    fn copy(base: &Path, key: &str) -> anyhow::Result<()> {
        std::fs::create_dir_all(base.join("mac").join(key).join("src"))?;
        std::fs::write(base.join("mac").join(key).join("src/a.rs"), "fn a() {}\n")?;
        Ok(())
    }

    /// The keeper's file beside the copies, as last written.
    fn saved(base: &Path) -> anyhow::Result<Saved> {
        Ok(serde_json::from_slice(&std::fs::read(base.join(SAVED))?)?)
    }

    /// Goal 9: the keeper looks on what changes what it decides, never on a clock (it looked every
    /// 20 s). Do: a keeper over one copy, its node never reaching the server, settled after its
    /// first look; a quiet span; then a use starting, the same use ending, the connection coming
    /// up, and the server's word on its rules. Expect: not one poll in the quiet span; a look for
    /// each event; the word's rules taken, and its file written with them.
    #[test]
    fn g9_an_idle_keeper_wakes_for_nothing_and_each_event_wakes_it() -> anyhow::Result<()> {
        // A bound that only ends a hang: a loaded machine takes what time it takes.
        let until = Instant::now() + Duration::from_secs(120);
        crate::rt::block_on(async move {
            let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
            let base = dir.path().to_path_buf();
            copy(&base, "kiln-0123456789")?;
            let using = Arc::new(Using::default());
            let handle = node(state.path(), &base, &using)?;
            let polls = Arc::new(AtomicU64::new(0));
            drop(crate::rt::spawn(crate::rt::counting_polls(Arc::clone(&polls), Keeper::new(handle, Arc::clone(&using), base.clone()).run())));
            crate::rt::settled(&polls, until).await?;
            anyhow::ensure!(using.looks() == 1, "{} looks first", using.looks());
            let quiet = polls.load(Ordering::Relaxed);
            crate::rt::sleep(Duration::from_secs(3)).await;
            let woke = polls.load(Ordering::Relaxed).saturating_sub(quiet);
            anyhow::ensure!(woke == 0 && using.looks() == 1, "{woke} polls, {} looks in 3 s, idle", using.looks());
            let mut looks = using.looks();
            let mut step = |what: &str, looks_now: u64| -> anyhow::Result<()> {
                anyhow::ensure!(looks_now > looks, "{what} didn't wake it ({looks_now} looks)");
                looks = looks_now;
                Ok(())
            };
            let busy = using.busy("mac", "kiln-0123456789", Some("agent-1".into()));
            crate::rt::settled(&polls, until).await?;
            step("a use starting", using.looks())?;
            drop(busy);
            crate::rt::settled(&polls, until).await?;
            step("a use ending", using.looks())?;
            using.connected();
            crate::rt::settled(&polls, until).await?;
            step("the connection up", using.looks())?;
            let rules = Set { system: keep::Rules { newest: Some(1), ..keep::Rules::default() }, ..Set::default() };
            using.told(keep::Answer { set: rules.clone(), asked: 0 });
            crate::rt::settled(&polls, until).await?;
            step("the server's word", using.looks())?;
            anyhow::ensure!(saved(&base)?.set == rules, "the word's rules taken and kept");
            Ok(())
        })
    }

    /// Goal 9: a runner process that doesn't keep the copies waits for `.keep.lock` on a thread,
    /// and is woken by its holder letting go (it asked again every 20 s). Do: the lock held
    /// elsewhere (here: another open file, as another process's would be); a keeper settled; the
    /// lock let go. Expect: the keeper holds it then, unasked, and writes its file.
    #[test]
    fn g9_the_lock_let_go_wakes_the_keeper_that_waited() -> anyhow::Result<()> {
        let until = Instant::now() + Duration::from_secs(120);
        crate::rt::block_on(async move {
            let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
            let base = dir.path().to_path_buf();
            copy(&base, "kiln-0123456789")?;
            let other = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(base.join(LOCK))?;
            other.lock()?;
            let using = Arc::new(Using::default());
            let handle = node(state.path(), &base, &using)?;
            let polls = Arc::new(AtomicU64::new(0));
            drop(crate::rt::spawn(crate::rt::counting_polls(Arc::clone(&polls), Keeper::new(handle, Arc::clone(&using), base.clone()).run())));
            crate::rt::settled(&polls, until).await?;
            anyhow::ensure!(using.looks() == 1 && !base.join(SAVED).exists(), "it doesn't keep them yet");
            drop(other);
            while !base.join(SAVED).exists() {
                anyhow::ensure!(Instant::now() < until, "the lock let go never woke it");
                crate::rt::sleep(Duration::from_millis(50)).await;
            }
            anyhow::ensure!(using.looks() == 2, "{} looks: woken once, by the lock", using.looks());
            anyhow::ensure!(saved(&base)?.records.contains_key("mac/kiln-0123456789"));
            Ok(())
        })
    }

    /// Goal 9: while the room is short, a copy held back (used too lately) is looked at when its
    /// grace ends, a due time of its own (the 20 s look found it before). Do: rules that want more
    /// free room than any disk has, one copy used 2 s short of its grace; a keeper. Expect: it
    /// stays at the first look, and goes at the second, which comes by itself, for room.
    #[test]
    fn g9_a_copy_held_back_while_the_room_is_short_goes_when_it_may() -> anyhow::Result<()> {
        let until = Instant::now() + Duration::from_secs(120);
        crate::rt::block_on(async move {
            let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
            let base = dir.path().to_path_buf();
            copy(&base, "held-0123456789")?;
            let now = crate::util::now_unix();
            let used = now - keep::GRACE_S + 2;
            let saved_before = Saved {
                since: now,
                set: Set { system: keep::Rules { free_gib: Some(1 << 20), ..keep::Rules::default() }, ..Set::default() },
                records: BTreeMap::from([("mac/held-0123456789".into(), Record { created: used, used, read: now, ..Record::default() })]),
                ..Saved::default()
            };
            std::fs::write(base.join(SAVED), serde_json::to_vec(&saved_before)?)?;
            let using = Arc::new(Using::default());
            let handle = node(state.path(), &base, &using)?;
            drop(crate::rt::spawn(Keeper::new(handle, Arc::clone(&using), base.clone()).run()));
            while using.looks() < 1 {
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            anyhow::ensure!(base.join("mac/held-0123456789").exists() || crate::util::now_unix() >= used + keep::GRACE_S, "held at first");
            while base.join("mac/held-0123456789").exists() {
                anyhow::ensure!(Instant::now() < until, "its grace's end never woke it");
                crate::rt::sleep(Duration::from_millis(50)).await;
            }
            // G9: removing the directory precedes saving the reason. Wait for that durable
            // record too; an empty older file's default trigger is Schedule, not this look's.
            let gone = loop {
                if let Some(gone) = saved(&base)?.gone.into_iter().find(|g| g.key == "held-0123456789") {
                    break gone;
                }
                anyhow::ensure!(Instant::now() < until, "its cleanup decision was never saved");
                crate::rt::sleep(Duration::from_millis(20)).await;
            };
            anyhow::ensure!(gone.trigger == keep::Trigger::Room && using.looks() == 2, "{:?} after {} looks", gone.trigger, using.looks());
            Ok(())
        })
    }

    /// Goal 9: a copy is measured once after each use of it ends (it was measured at most every 10
    /// minutes), and not again until another does. Do: one copy whose record was sized after its
    /// last use (a size written by hand), one sized before it; a look. Expect: the first keeps its
    /// size, the second is measured.
    #[test]
    fn g9_a_copy_is_measured_after_a_use_ends_and_not_again() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (dir, state) = (tempfile::tempdir()?, tempfile::tempdir()?);
            let base = dir.path().to_path_buf();
            copy(&base, "sized-0123456789")?;
            copy(&base, "used-0123456789")?;
            let now = crate::util::now_unix();
            let record = |used: i64, sized: i64| Record { created: now - 100, used, read: now, bytes: Some(1), sized, ..Record::default() };
            let before = Saved { since: now, records: BTreeMap::from([("mac/sized-0123456789".into(), record(now - 50, now - 10)), ("mac/used-0123456789".into(), record(now - 10, now - 50))]), ..Saved::default() };
            std::fs::write(base.join(SAVED), serde_json::to_vec(&before)?)?;
            let using = Arc::new(Using::default());
            let mut k = Keeper::new(node(state.path(), &base, &using)?, using, base.clone());
            let _ = k.look().await;
            let bytes = |id: &str| k.saved.records.get(id).and_then(|r| r.bytes);
            anyhow::ensure!(bytes("mac/sized-0123456789") == Some(1), "measured since its last use: not again");
            anyhow::ensure!(bytes("mac/used-0123456789").is_some_and(|b| b > 1), "used since it was measured: measured");
            Ok(())
        })
    }

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
            let handle = node(state.path(), &base, &using)?;
            let mut k = Keeper::new(handle, using, base.clone());
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
