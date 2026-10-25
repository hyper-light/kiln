//! A runner's git repositories (`docs/sync.md`, "Repositories on machines"): it says what this
//! machine holds when it connects, when that changes and every few minutes ([`Msg::Repos`]), and
//! does its part of the server's tasks: bundling a repository for clones elsewhere, cloning one,
//! deleting the folder of one moved away. Each says how it goes as it goes ([`Msg::Task`]), again
//! every few seconds while it works and again after a reconnect, so the server knows it is alive.
//!
//! Bundles travel over HTTP to and from the server (`/api/sync/bundles/:batch`), with the token the
//! server gives this machine with its copies: the same links, the same ports.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};

use super::git::{self, Kept, Registry};
use super::wire::{self, CloneOrder, Msg, Repos, ShareSlot, TaskNews, Work};
use crate::http::client::Client;
use crate::proto::AgentMsg;

/// How often this machine looks at its repositories when nothing asks it to, and says so even if
/// nothing changed.
const LOOK_EVERY: Duration = Duration::from_secs(300);
const SAY_EVERY: Duration = Duration::from_secs(1800);
/// News of a task at work is said again this often; news of one over is kept this long, to say
/// again after a reconnect.
const AGAIN: Duration = Duration::from_secs(15);
const KEEP_NEWS: Duration = Duration::from_secs(900);
/// Progress while bytes move is said at most this often.
const PROGRESS: Duration = Duration::from_secs(3);
/// The largest bundle this machine fetches.
const BUNDLE_MAX: u64 = 64 << 30;

/// What the runner's sync hands the worker.
pub enum Job {
    /// What the server said: `Look`, `Bundle`, `Clone`, `Delete` or `Forget`.
    Order(Msg),
    /// The token blob and bundle requests carry.
    Token(String),
    /// The folders of this machine's synced copies (looked at wherever they are).
    Paths(Vec<String>),
    Connected(bool),
    /// What a look found: looks go on beside the orders, never in their way.
    Looked(git::Looked),
}

/// How the worker looks at the machine's repositories: [`git::look`], but for tests.
type Scan = Arc<dyn Fn(PathBuf, Vec<PathBuf>, BTreeSet<String>) -> std::pin::Pin<Box<dyn std::future::Future<Output = git::Looked> + Send>> + Send + Sync>;

/// What this machine's work in a repository was when it last went: its branches' and its folders'
/// fingerprints, and when.
struct Sent {
    branches: String,
    files: String,
    at: Instant,
}

/// A repository's work goes at most this often (a new slot from the server aside).
const SHARE_EVERY: Duration = Duration::from_secs(120);
/// Its folders' work in progress goes for those git worked in this recently, the latest first, at most so many.
const WIP_RECENT: i64 = 7 * 86_400;
const WIP_FOLDERS: usize = 16;

/// What the worker and the tasks it runs share.
struct Shared {
    dir: PathBuf,
    me: String,
    home: PathBuf,
    http: Arc<Client>,
    server: String,
    say: async_channel::Sender<AgentMsg>,
    connected: AtomicBool,
    token: Mutex<String>,
    news: Mutex<HashMap<String, (TaskNews, Instant)>>,
    registry: Mutex<Registry>,
    /// The worker's own queue, to look again once a task is done.
    kick: async_channel::Sender<Job>,
    /// Folders a task is cloning into or deleting now: never two at once.
    busy: Mutex<BTreeSet<String>>,
    scan: Scan,
    /// The repositories others hold too, and where this machine's work in each goes next
    /// (`super::share`); its folders of each, as it last looked, with when git last worked there;
    /// what its work in each was when it last shared it; and whether it is sharing now.
    slots: Mutex<Vec<ShareSlot>>,
    folders: Mutex<HashMap<String, Vec<(PathBuf, i64)>>>,
    shared: Mutex<HashMap<String, Sent>>,
    sharing: AtomicBool,
}

impl Shared {
    fn send(&self, msg: &Msg) {
        if self.connected.load(Ordering::SeqCst) {
            let _ = self.say.try_send(AgentMsg::Sync { json: wire::encode(msg), raft: Vec::new() });
        }
    }

    /// Says how a task goes, and keeps it to say again.
    fn tell(&self, news: TaskNews) {
        self.news.lock().insert(news.task.clone(), (news.clone(), Instant::now()));
        self.send(&Msg::Task(news));
    }

    fn token(&self) -> String {
        self.token.lock().clone()
    }

    fn registry_file(&self) -> PathBuf {
        self.dir.join("clones.json")
    }

    fn bundle_file(&self, name: &str) -> PathBuf {
        let safe: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        self.dir.join("bundles").join(format!("{safe}.bundle"))
    }

    fn url(&self, batch: &str) -> String {
        let enc = |s: &str| -> String { s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect() };
        format!("{}/api/sync/bundles/{}?machine={}", self.server.trim_end_matches('/'), enc(batch), enc(&self.me))
    }
}

/// Starts this machine's worker (its state in `dir`, `<state>/sync`); what it says goes out through
/// `say`. Hand it [`Job`]s.
pub fn start(dir: PathBuf, me: String, http: Arc<Client>, server: String, say: async_channel::Sender<AgentMsg>) -> async_channel::Sender<Job> {
    let scan: Scan = Arc::new(|home, extra, kiln| Box::pin(git::look(home, extra, kiln)));
    start_with(dir, me, http, server, say, crate::os::home_dir(), scan)
}

fn start_with(dir: PathBuf, me: String, http: Arc<Client>, server: String, say: async_channel::Sender<AgentMsg>, home: PathBuf, scan: Scan) -> async_channel::Sender<Job> {
    let (tx, rx) = async_channel::unbounded();
    let registry = Registry::load(&dir.join("clones.json"));
    let shared = Arc::new(Shared {
        dir,
        me,
        home,
        http,
        server,
        say,
        connected: AtomicBool::new(false),
        token: Mutex::new(String::new()),
        news: Mutex::new(HashMap::new()),
        registry: Mutex::new(registry),
        kick: tx.clone(),
        busy: Mutex::new(BTreeSet::new()),
        scan,
        slots: Mutex::new(Vec::new()),
        folders: Mutex::new(HashMap::new()),
        shared: Mutex::new(HashMap::new()),
        sharing: AtomicBool::new(false),
    });
    crate::rt::spawn(async move { Worker { s: shared, paths: Vec::new(), told: None, told_at: None, looked: None, looking: false, soon: true }.run(rx).await });
    tx
}

struct Worker {
    s: Arc<Shared>,
    paths: Vec<String>,
    /// What it last said it holds, and when.
    told: Option<Repos>,
    told_at: Option<Instant>,
    looked: Option<Instant>,
    /// A look is under way.
    looking: bool,
    /// Look at the next chance.
    soon: bool,
}

impl Worker {
    async fn run(mut self, rx: async_channel::Receiver<Job>) {
        loop {
            match crate::rt::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(job)) => self.job(job),
                Ok(Err(_)) => return,
                Err(_) => {}
            }
            while let Ok(job) = rx.try_recv() {
                self.job(job);
            }
            self.again();
            let due = self.looked.is_none_or(|t| t.elapsed() >= LOOK_EVERY);
            if self.s.connected.load(Ordering::SeqCst) && (self.soon || due) && !self.looking {
                self.soon = false;
                self.look();
            }
        }
    }

    fn job(&mut self, job: Job) {
        match job {
            Job::Token(t) => *self.s.token.lock() = t,
            Job::Paths(p) => {
                if p != self.paths {
                    self.paths = p;
                    self.soon = true;
                }
            }
            Job::Connected(up) => {
                self.s.connected.store(up, Ordering::SeqCst);
                if up {
                    // All of it again: the server may have restarted.
                    self.told = None;
                    self.soon = true;
                    let all: Vec<TaskNews> = self.s.news.lock().values().map(|(n, _)| n.clone()).collect();
                    for n in all {
                        self.s.send(&Msg::Task(n));
                    }
                }
            }
            Job::Order(Msg::Look) => self.soon = true,
            Job::Looked(looked) => self.report(looked),
            Job::Order(Msg::Shares { shares }) => {
                // Only new slots (a repository, a fresh one) start a share; looks do the rest.
                let changed = *self.s.slots.lock() != shares;
                *self.s.slots.lock() = shares;
                if changed {
                    share_soon(&self.s);
                }
            }
            Job::Order(Msg::Take(w)) => {
                let s = self.s.clone();
                crate::rt::spawn(async move { take_work(&s, *w).await });
            }
            Job::Order(Msg::Forget { path }) => {
                let changed = self.s.registry.lock().clones.remove(&path).is_some();
                if changed {
                    let r = self.s.registry.lock().clone();
                    if let Err(e) = r.save(&self.s.registry_file()) {
                        tracing::warn!(error = %format!("{e:#}"), "the clones kiln made here could not be saved");
                    }
                    self.soon = true;
                }
            }
            Job::Order(Msg::Bundle { task, path }) => {
                let s = self.s.clone();
                crate::rt::spawn(async move { bundle(&s, &task, &path).await });
            }
            Job::Order(Msg::Clone(order)) => {
                let s = self.s.clone();
                crate::rt::spawn(async move { clone(&s, order).await });
            }
            Job::Order(Msg::Delete { task, path, origin }) => {
                if self.paths.contains(&path) {
                    self.s.tell(TaskNews::failed(&task, format!("{path} is still synced here")));
                    return;
                }
                let s = self.s.clone();
                crate::rt::spawn(async move { delete(&s, &task, &path, origin.as_deref()).await });
            }
            // The rest are the copies' (`crate::sync::runner`).
            Job::Order(_) => {}
        }
    }

    /// Says again what tasks at work are doing; forgets news of tasks over long ago.
    fn again(&self) {
        let mut out = Vec::new();
        {
            let mut news = self.s.news.lock();
            news.retain(|_, (n, at)| !n.over() || at.elapsed() < KEEP_NEWS);
            for (n, at) in news.values_mut() {
                if !n.over() && at.elapsed() >= AGAIN {
                    *at = Instant::now();
                    out.push(n.clone());
                }
            }
        }
        for n in out {
            self.s.send(&Msg::Task(n));
        }
    }

    /// Starts looking at this machine's repositories. A look runs git in each one, which takes
    /// minutes on a busy machine with many: the orders that come meanwhile go on at once.
    fn look(&mut self) {
        let (extra, kiln) = {
            let r = self.s.registry.lock();
            let kiln: BTreeSet<String> = r.clones.keys().cloned().collect();
            let mut extra: Vec<PathBuf> = r.paths().collect();
            extra.extend(self.paths.iter().map(|p| git::expand(p, &self.s.home)));
            (extra, kiln)
        };
        self.looking = true;
        let (looking, kick) = ((self.s.scan)(self.s.home.clone(), extra, kiln), self.s.kick.clone());
        crate::rt::spawn(async move {
            let _ = kick.send(Job::Looked(looking.await)).await;
        });
    }

    /// Says what a look found, if that changed (or it is time).
    fn report(&mut self, looked: git::Looked) {
        self.looking = false;
        self.looked = Some(Instant::now());
        let report = Repos { home: self.s.home.to_string_lossy().into_owned(), repos: looked.repos, error: looked.error, at: crate::util::now_unix() };
        // Its folders of each repository with a remote, for sharing its work and taking others'.
        let mut folders: HashMap<String, Vec<(PathBuf, i64)>> = HashMap::new();
        for f in &report.repos {
            if let Some(key) = f.origin.as_deref().and_then(super::places::key_of) {
                folders.entry(key).or_default().push((PathBuf::from(&f.path), f.active));
            }
        }
        *self.s.folders.lock() = folders;
        share_soon(&self.s);
        let same = self.told.as_ref().is_some_and(|t| Repos { at: report.at, ..t.clone() } == report);
        let stale = self.told_at.is_none_or(|t| t.elapsed() >= SAY_EVERY);
        if !same || stale {
            self.s.send(&Msg::Repos(report.clone()));
            self.told = Some(report);
            self.told_at = Some(Instant::now());
        }
    }
}

/// A size as people read it.
fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = "KB";
    for u in UNITS.iter().skip(1) {
        if v < 1024.0 {
            break;
        }
        v /= 1024.0;
        unit = u;
    }
    format!("{v:.1} {unit}")
}

/// Takes a folder for one task at a time; none: another task has it.
struct Busy<'a> {
    s: &'a Shared,
    path: String,
}

impl<'a> Busy<'a> {
    fn take(s: &'a Shared, path: &str) -> Option<Self> {
        s.busy.lock().insert(path.to_owned()).then(|| Self { s, path: path.to_owned() })
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.s.busy.lock().remove(&self.path);
    }
}

/// Bundles the repository at `path` and sends it to the server for batch `task`.
async fn bundle(s: &Arc<Shared>, task: &str, path: &str) {
    s.tell(TaskNews::new(task, "bundling", format!("bundling the commits in {path}")));
    let file = s.bundle_file(task);
    let out = async {
        if let Some(dir) = file.parent() {
            let d = dir.to_path_buf();
            crate::rt::blocking(move || std::fs::create_dir_all(d)).await??;
        }
        let _ = std::fs::remove_file(&file);
        git::bundle(&git::expand(path, &s.home), &file).await?;
        let size = std::fs::metadata(&file)?.len();
        s.tell(TaskNews::new(task, "sending", format!("sending the bundle ({})", bytes(size))));
        send_bundle(s, Some(task), task, &file, size).await
    }
    .await;
    let _ = std::fs::remove_file(&file);
    match out {
        Ok((hash, size)) => s.tell(TaskNews { hash: Some(hash), size: Some(size), ..TaskNews::new(task, "done", format!("sent the bundle ({})", bytes(size))) }),
        Err(e) => s.tell(TaskNews::failed(task, format!("{e:#}"))),
    }
}

/// Sends a bundle to the server for batch `batch`, saying how it goes on task `task`, if one.
async fn send_bundle(s: &Arc<Shared>, task: Option<&str>, batch: &str, file: &Path, size: u64) -> anyhow::Result<(String, u64)> {
    let src = crate::rt::file_source(file.to_path_buf())?;
    let (tx, rx) = async_channel::bounded(8);
    let (s2, t2) = (s.clone(), task.map(str::to_owned));
    crate::rt::spawn(async move {
        let (mut sent, mut said) = (0u64, Instant::now());
        while let Ok(chunk) = src.recv().await {
            sent = sent.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
            if let Some(t) = &t2
                && said.elapsed() >= PROGRESS
            {
                said = Instant::now();
                s2.tell(TaskNews::new(t, "sending", format!("sent {} of {}", bytes(sent), bytes(size))));
            }
            if tx.send(chunk).await.is_err() {
                break;
            }
        }
    });
    let resp = s.http.put(&s.url(batch)).bearer(&s.token()).header("content-type", "application/octet-stream").body_stream(rx).timeout(Duration::from_secs(4 * 3600)).send().await?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        let body = resp.bytes(64 << 10).await.unwrap_or_default();
        anyhow::bail!("the server took no bundle ({status}): {}", String::from_utf8_lossy(&body).trim());
    }
    #[derive(serde::Deserialize)]
    struct Got {
        hash: String,
        size: u64,
    }
    let got: Got = resp.json(64 << 10).await?;
    Ok((got.hash, got.size))
}

/// Fetches a batch's bundle into `file`, checking it is the one named.
async fn fetch_bundle(s: &Arc<Shared>, task: Option<&str>, batch: &str, hash: &str, size: u64, file: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(size <= BUNDLE_MAX, "the bundle is {}: more than this machine fetches", bytes(size));
    let mut resp = s.http.get(&s.url(batch)).bearer(&s.token()).timeout(Duration::from_secs(900)).send().await?;
    anyhow::ensure!(resp.is_success(), "the server gave no bundle ({})", resp.status());
    let (sink, done) = crate::rt::file_sink(file.to_path_buf(), BUNDLE_MAX)?;
    let mut h = Sha256::new();
    let (mut got, mut said) = (0u64, Instant::now());
    let mut whole = true;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                h.update(&chunk);
                got = got.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                if let Some(task) = task
                    && said.elapsed() >= PROGRESS
                {
                    said = Instant::now();
                    s.tell(TaskNews::new(task, "fetching", format!("fetched {} of {}", bytes(got), bytes(size))));
                }
                if sink.send(chunk).await.is_err() {
                    whole = false;
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => anyhow::bail!("the bundle stopped coming: {e:#}"),
        }
    }
    drop(sink);
    done.await?;
    anyhow::ensure!(whole, "the bundle could not be written");
    anyhow::ensure!(hex::encode(h.finalize()) == hash, "the bundle that came is not the one sent");
    Ok(())
}

/// Clones what `order` says, and keeps the clone as kiln's.
async fn clone(s: &Arc<Shared>, order: CloneOrder) {
    let task = order.task.clone();
    let path = git::expand(&order.path, &s.home).to_string_lossy().into_owned();
    let Some(_busy) = Busy::take(s, &path) else {
        s.tell(TaskNews::failed(&task, format!("another task is at {path} now")));
        return;
    };
    let bundle = s.bundle_file(&task);
    let out = async {
        let source: OsString = match (&order.bundle, &order.hash, &order.url) {
            (Some(batch), Some(hash), _) => {
                s.tell(TaskNews::new(&task, "fetching", format!("fetching the bundle ({})", bytes(order.size))));
                fetch_bundle(s, Some(&task), batch, hash, order.size, &bundle).await?;
                bundle.clone().into_os_string()
            }
            (_, _, Some(url)) => url.into(),
            _ => anyhow::bail!("nothing to clone from"),
        };
        s.tell(TaskNews::new(&task, "cloning", format!("cloning into {path}")));
        git::clone_into(&order, &source, &s.home).await
    }
    .await;
    let _ = std::fs::remove_file(&bundle);
    match out {
        Ok((branch, head)) => {
            let saved = {
                let mut r = s.registry.lock();
                r.clones.insert(path.clone(), Kept { key: order.key.clone(), task: task.clone(), at: crate::util::now_unix() });
                r.clone()
            };
            if let Err(e) = saved.save(&s.registry_file()) {
                tracing::warn!(error = %format!("{e:#}"), "the clones kiln made here could not be saved");
            }
            let what = match &branch {
                Some(b) => format!("cloned {b} into {path}"),
                None => format!("cloned into {path}"),
            };
            s.tell(TaskNews { branch, head, ..TaskNews::new(&task, "done", what) });
            let _ = s.kick.try_send(Job::Order(Msg::Look));
        }
        Err(e) => s.tell(TaskNews::failed(&task, format!("{e:#}"))),
    }
}

/// Shares this machine's work in the repositories others hold too, unless it is already at it.
fn share_soon(s: &Arc<Shared>) {
    if s.sharing.swap(true, Ordering::SeqCst) {
        return;
    }
    let s = s.clone();
    crate::rt::spawn(async move {
        share_all(&s).await;
        s.sharing.store(false, Ordering::SeqCst);
    });
}

/// For each repository others hold too, this machine's work in it, if it changed since it last
/// went (or the server has none of it): from the repository among its folders of it that git last
/// worked in. Its branches' bundle goes when they moved; its folders' work in progress (the ones git
/// worked in lately) when it changed.
async fn share_all(s: &Arc<Shared>) {
    let slots = s.slots.lock().clone();
    let now = crate::util::now_unix();
    for slot in slots {
        let Some(mine) = s.folders.lock().get(&slot.key).cloned() else { continue };
        let recent = s.shared.lock().get(&slot.key).is_some_and(|sent| sent.at.elapsed() < SHARE_EVERY);
        if recent && !slot.fresh {
            continue;
        }
        let mut repos: BTreeMap<PathBuf, (i64, Vec<(PathBuf, i64)>)> = BTreeMap::new();
        for (f, active) in mine {
            let Some(r) = super::share::repository_of(&f).await else { continue };
            let e = repos.entry(r).or_default();
            e.0 = e.0.max(active);
            e.1.push((f, active));
        }
        let Some((repo, (_, mut group))) = repos.into_iter().max_by_key(|(_, (active, _))| *active) else { continue };
        group.retain(|(_, active)| now.saturating_sub(*active) <= WIP_RECENT);
        group.sort_by_key(|(_, active)| std::cmp::Reverse(*active));
        group.truncate(WIP_FOLDERS);
        let folders: Vec<PathBuf> = group.into_iter().map(|(f, _)| f).collect();
        let (branches_print, files_print) = super::share::fingerprints(&repo, &folders).await;
        let before = s.shared.lock().get(&slot.key).map(|sent| (sent.branches.clone(), sent.files.clone()));
        let branches_moved = slot.fresh || before.as_ref().is_none_or(|(b, _)| *b != branches_print);
        let files_changed = slot.fresh || before.as_ref().is_none_or(|(_, f)| *f != files_print);
        if !branches_moved && !files_changed {
            continue;
        }
        let (file, wip_file) = (s.bundle_file(&format!("share-{}", slot.batch)), s.bundle_file(&format!("share-{}", slot.wip_batch)));
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let out = async {
            let mut w = Work { key: slot.key.clone(), branches: super::share::branches(&repo).await?, wip: super::share::wips(&folders, &s.me).await, ..Work::default() };
            if branches_moved {
                w.batch.clone_from(&slot.batch);
                if super::share::bundle_branches(&repo, &w.branches, &file).await? {
                    let size = std::fs::metadata(&file)?.len();
                    send_bundle(s, None, &slot.batch, &file, size).await?;
                }
            }
            if files_changed {
                w.wip_batch.clone_from(&slot.wip_batch);
                if super::share::bundle_wip(&repo, &w.wip, &w.branches, &wip_file).await? {
                    let size = std::fs::metadata(&wip_file)?.len();
                    send_bundle(s, None, &slot.wip_batch, &wip_file, size).await?;
                }
            }
            anyhow::Ok(w)
        }
        .await;
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&wip_file);
        match out {
            Ok(w) => {
                s.shared.lock().insert(slot.key.clone(), Sent { branches: branches_print, files: files_print, at: Instant::now() });
                s.send(&Msg::Shared(Box::new(w)));
            }
            Err(e) => tracing::warn!(target: "kiln.sync", repo = %slot.key, error = %format!("{e:#}"), "this machine's work couldn't be shared"),
        }
    }
}

/// Takes another machine's work into each repository of it this machine holds.
async fn take_work(s: &Arc<Shared>, w: Work) {
    let Some(mine) = s.folders.lock().get(&w.key).cloned() else { return };
    let mut repos: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for (f, _) in mine {
        if let Some(r) = super::share::repository_of(&f).await {
            repos.entry(r).or_insert(f);
        }
    }
    let mut files = Vec::new();
    for (batch, hash, size) in [(&w.batch, &w.hash, w.size), (&w.wip_batch, &w.wip_hash, w.wip_size)] {
        let Some(hash) = hash else { continue };
        let file = s.bundle_file(&format!("take-{batch}"));
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match fetch_bundle(s, None, batch, hash, size, &file).await {
            Ok(()) => files.push(file),
            Err(e) => tracing::warn!(target: "kiln.sync", repo = %w.key, from = %w.from, error = %format!("{e:#}"), "another machine's work couldn't be fetched"),
        }
    }
    let bundles: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    for folder in repos.values() {
        if let Err(e) = super::share::take(folder, &w, &bundles).await {
            tracing::warn!(target: "kiln.sync", repo = %w.key, from = %w.from, folder = %folder.display(), error = %format!("{e:#}"), "another machine's work couldn't be taken in");
        }
    }
    for f in &files {
        let _ = std::fs::remove_file(f);
    }
}

/// Deletes the folder of a repository moved away (or given up), if it is safe to.
async fn delete(s: &Arc<Shared>, task: &str, path: &str, origin: Option<&str>) {
    let Some(_busy) = Busy::take(s, path) else {
        s.tell(TaskNews::failed(task, format!("another task is at {path} now")));
        return;
    };
    s.tell(TaskNews::new(task, "deleting", format!("deleting {path}")));
    // Never kiln's own folders: its state, and where its binary is.
    let mut keep: Vec<PathBuf> = s.dir.parent().map(Path::to_path_buf).into_iter().collect();
    keep.extend(std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)));
    match git::delete(&git::expand(path, &s.home), origin, &s.home, &keep).await {
        Ok(()) => {
            let saved = {
                let mut r = s.registry.lock();
                r.clones.remove(path).map(|_| r.clone())
            };
            if let Some(r) = saved
                && let Err(e) = r.save(&s.registry_file())
            {
                tracing::warn!(error = %format!("{e:#}"), "the clones kiln made here could not be saved");
            }
            s.tell(TaskNews::new(task, "done", format!("deleted {path}")));
            let _ = s.kick.try_send(Job::Order(Msg::Look));
        }
        Err(e) => s.tell(TaskNews::failed(task, format!("{e:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_goes_on_at_once_while_a_slow_look_is_under_way() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (src, home) = (dir.path().join("src"), dir.path().join("home"));
        std::fs::create_dir_all(&src)?;
        std::fs::create_dir_all(&home)?;
        for args in [&["init", "-q", "-b", "main"][..], &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "--allow-empty", "-m", "first"]] {
            let mut git = std::process::Command::new("git");
            git.arg("-C").arg(&src).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1");
            anyhow::ensure!(git.status()?.success(), "git {}", args.join(" "));
        }
        crate::rt::block_on(async move {
            // A look that starts, then waits until the test lets it end.
            let (started_tx, started) = async_channel::unbounded::<()>();
            let (end, ended) = async_channel::bounded::<()>(1);
            let scan: Scan = Arc::new(move |_, _, _| {
                let (started_tx, ended) = (started_tx.clone(), ended.clone());
                Box::pin(async move {
                    let _ = started_tx.send(()).await;
                    let _ = ended.recv().await;
                    git::Looked { repos: Vec::new(), error: None }
                })
            });
            let (say, heard) = async_channel::unbounded();
            let http = Arc::new(Client::new(crate::http::client::Roots::WebPki, None)?);
            let jobs = start_with(dir.path().join("state"), "me".into(), http, "http://127.0.0.1:9".into(), say, home.clone(), scan);
            jobs.send(Job::Connected(true)).await?;
            crate::rt::timeout(Duration::from_secs(10), started.recv()).await??;
            let order = CloneOrder { task: "t1".into(), key: "k".into(), path: home.join("b").to_string_lossy().into_owned(), url: Some(src.to_string_lossy().into_owned()), ..CloneOrder::default() };
            jobs.send(Job::Order(Msg::Clone(order))).await?;
            let next = || async {
                loop {
                    let AgentMsg::Sync { json, .. } = heard.recv().await? else { continue };
                    match wire::decode(&json) {
                        Some(Msg::Task(n)) if n.over() => return anyhow::Ok(Some(n)),
                        Some(Msg::Repos(_)) => return Ok(None),
                        _ => {}
                    }
                }
            };
            let done = crate::rt::timeout(Duration::from_secs(30), next()).await??.ok_or_else(|| anyhow::anyhow!("the look ended before the clone"))?;
            assert_eq!((done.state.as_str(), done.error, done.branch.as_deref()), ("done", None, Some("main")));
            assert!(home.join("b/.git").exists());
            // The look ends, and says what it found.
            end.send(()).await?;
            anyhow::ensure!(crate::rt::timeout(Duration::from_secs(10), next()).await??.is_none(), "the look said what it found");
            Ok(())
        })
    }

    #[test]
    fn sizes_read_as_people_say_them() {
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(2048), "2.0 KB");
        assert_eq!(bytes(5 << 20), "5.0 MB");
        assert_eq!(bytes(3 << 30), "3.0 GB");
    }
}
