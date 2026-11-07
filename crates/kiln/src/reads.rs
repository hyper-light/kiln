//! The API's reads of kiln's records, answered from a store: the server's kiln.db, or a runner's
//! replica of it, where the runner's front door answers the owner's (docs/p2p.md, P2). The server's
//! routes call these too, so an answer is the same wherever it's read.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use serde_json::{Value, json};

use crate::store::{Repo, Run, Store};

/// What a read did while it was answered (goal 10, plan 01a1a87b2dae step 2's trace): the values it
/// encoded as JSON and the calls and reads it asked of a store. Each is counted where it happens, on
/// the thread it runs on; [`trace::traced`] runs a read and takes what each of its polls counted (a
/// poll runs alone on its thread), so the counts are the read's own, whatever runs beside it. Counted
/// in tests alone: elsewhere these are nothing.
pub mod trace {
    /// What a read did while it was answered.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Counts {
        /// Values encoded as JSON: `srv::json`'s answers, and views made then and there.
        pub encoded: u64,
        /// Calls and reads asked of a store: kiln.db's (`crate::store::Store`), the conversations'.
        pub asked: u64,
    }

    #[cfg(test)]
    thread_local! {
        static NOW: std::cell::Cell<Counts> = const { std::cell::Cell::new(Counts { encoded: 0, asked: 0 }) };
    }

    /// A value encoded as JSON, here and now.
    #[inline]
    pub fn encoded() {
        #[cfg(test)]
        NOW.with(|n| n.set(Counts { encoded: n.get().encoded.wrapping_add(1), ..n.get() }));
    }

    /// A store asked for a call or a read, here and now.
    #[inline]
    pub fn asked() {
        #[cfg(test)]
        NOW.with(|n| n.set(Counts { asked: n.get().asked.wrapping_add(1), ..n.get() }));
    }

    /// `fut`'s output, and what it encoded and asked of a store while it ran.
    #[cfg(test)]
    pub async fn traced<F: Future>(fut: F) -> (F::Output, Counts) {
        let mut fut = std::pin::pin!(fut);
        let mut took = Counts::default();
        let out = std::future::poll_fn(|cx| {
            let before = NOW.with(std::cell::Cell::get);
            let polled = fut.as_mut().poll(cx);
            let after = NOW.with(std::cell::Cell::get);
            took.encoded = took.encoded.wrapping_add(after.encoded.wrapping_sub(before.encoded));
            took.asked = took.asked.wrapping_add(after.asked.wrapping_sub(before.asked));
            polled
        })
        .await;
        (out, took)
    }
}

/// `value` as JSON, encoded here and now: how a view is made ([`trace`] counts it).
pub fn encode<T: serde::Serialize + ?Sized>(value: &T) -> anyhow::Result<bytes::Bytes> {
    trace::encoded();
    Ok(bytes::Bytes::from(serde_json::to_vec(value)?))
}

/// What a read keeps in memory of some of the store's tables, encoded (goal 10, plan 01a1a87b2dae step
/// 2): it answers only while those tables' count ([`crate::store::Changes`]) is the one it was made at,
/// so it never says less than the store would. Once they move, the read that finds it so is answered
/// from the store, as before, and it's made again in the background, one making at a time, off the
/// runtime's threads (on a read connection of the store's: `Store::read`).
pub struct Kept<T> {
    tables: &'static [&'static str],
    /// What it holds, and the tables' count it was made at.
    now: arc_swap::ArcSwapOption<(u64, T)>,
    making: AtomicBool,
    /// Told each time it's made again: the maker of the reads kept ahead that are made of it.
    made: Option<Arc<crate::rt::Notify>>,
}

impl<T: Send + Sync + 'static> Kept<T> {
    pub fn new(tables: &'static [&'static str]) -> Self {
        Self { tables, now: arc_swap::ArcSwapOption::empty(), making: AtomicBool::new(false), made: None }
    }

    /// One that tells `made` each time it's made again.
    fn telling(tables: &'static [&'static str], made: &Arc<crate::rt::Notify>) -> Self {
        Self { made: Some(Arc::clone(made)), ..Self::new(tables) }
    }

    /// What it holds, while it's what the store's tables hold; else none, and it's made again with
    /// `make` (unless that's under way), for the reads after this one.
    pub fn fresh<F>(self: &Arc<Self>, store: &Store, make: F) -> Option<Arc<(u64, T)>>
    where
        F: FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    {
        self.fresh_while(store, |_| true, make)
    }

    /// [`Kept::fresh`], for what holds only for a while whatever's written (the overview's figures of
    /// the last day): what it holds while it's what the store's tables hold and `holds` says it still
    /// holds now; else none, and it's made again.
    pub fn fresh_while<F>(self: &Arc<Self>, store: &Store, holds: impl Fn(&T) -> bool, make: F) -> Option<Arc<(u64, T)>>
    where
        F: FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    {
        let at = store.changes().of(self.tables);
        if let Some(now) = self.now.load_full().filter(|now| now.0 == at && holds(&now.1)) {
            return Some(now);
        }
        if !self.making.swap(true, Ordering::AcqRel) {
            let (kept, store) = (Arc::clone(self), store.clone());
            drop(crate::rt::spawn(async move {
                // Where the tables stand before they're read: a write after this makes it stale again.
                let at = store.changes().of(kept.tables);
                match store.read(make).await {
                    Ok(made) => {
                        kept.now.store(Some(Arc::new((at, made))));
                        if let Some(told) = &kept.made {
                            told.notify();
                        }
                    }
                    Err(e) => tracing::debug!(error = %format!("{e:#}"), "a read kept in memory couldn't be made: the store answers"),
                }
                kept.making.store(false, Ordering::Release);
            }));
        }
        None
    }
}

/// A read kept encoded ahead of its asking (goal 10, plan 01a1a87b2dae step 2), for what's read from
/// memory but changes by itself (the runners' live records, the server's uptime). What it holds is
/// keyed by what it was made of (`K`: the copies it read, each by identity, a copy swapped in whole
/// being a change, and the few values it says), taken again at each read, cheaply, so it answers only
/// while it says what a read made now would. Its maker ([`Views::start`]) makes it again as soon as
/// something it's made of changes, off the request path; a read that comes between the change and
/// that making has it made, once, for the reads after it: where it's cheap, on its own shard ([`Ahead::answer`],
/// counted: [`trace`]), else on the compute pool ([`Ahead::held`], then [`Ahead::keep`]). Each is held named
/// ([`Made`]).
pub struct Ahead<K> {
    now: arc_swap::ArcSwapOption<(K, Made)>,
    /// It's been read: its maker keeps it made.
    asked: AtomicBool,
    /// It's been read since its maker last looked ([`Ahead::take_read`]): what changes by itself (the overview's uptime)
    /// is made again as it changes only while it is (goal 9, plan 01a1a87b2cec step 5).
    read: AtomicBool,
    /// Told at its first read since its maker last looked: its maker keeps it made again.
    maker: Option<Arc<crate::rt::Notify>>,
}

/// A read kept ahead as it answers: its JSON, and the weak ETag that names it (`crate::http::compress::weak_etag` of
/// its digest, the very one `crate::http::compress::finish` makes of a body), made once with it, so the reads that
/// find it as it was aren't digested again each (goal 9, plan 01a1a87b2cec step 3: `GET /api/agents`, its runners'
/// records and facts, was digested whole on the request's shard at every read).
#[derive(Debug, Clone)]
pub struct Made {
    pub json: bytes::Bytes,
    pub etag: Option<http::HeaderValue>,
}

impl Made {
    /// `json`, named.
    pub fn of(json: bytes::Bytes) -> Self {
        let etag = crate::http::compress::weak_etag(&crate::http::compress::digest(&json));
        Self { json, etag }
    }
}

impl<K> Default for Ahead<K> {
    fn default() -> Self {
        Self { now: arc_swap::ArcSwapOption::empty(), asked: AtomicBool::new(false), read: AtomicBool::new(false), maker: None }
    }
}

impl<K> Ahead<K> {
    /// One whose first read since its maker last looked tells `maker`.
    fn telling(maker: &Arc<crate::rt::Notify>) -> Self {
        Self { maker: Some(Arc::clone(maker)), ..Self::default() }
    }

    /// A read: it's asked for, and read since its maker last looked (its maker told, the first time).
    fn note_read(&self) {
        self.asked.store(true, Ordering::Relaxed);
        if !self.read.swap(true, Ordering::Relaxed)
            && let Some(maker) = &self.maker
        {
            maker.notify();
        }
    }

    /// Whether it's been read since its maker last asked, asked now.
    pub fn take_read(&self) -> bool {
        self.read.swap(false, Ordering::Relaxed)
    }
}

impl<K: PartialEq> Ahead<K> {
    /// What it holds, made of `key`; else what `make` makes of `key` now, named and kept for the reads after
    /// this one.
    pub fn answer(&self, key: K, make: impl FnOnce(&K) -> anyhow::Result<bytes::Bytes>) -> anyhow::Result<Made> {
        self.note_read();
        if let Some(now) = self.now.load_full().filter(|now| now.0 == key) {
            return Ok(now.1.clone());
        }
        let made = Made::of(make(&key)?);
        self.now.store(Some(Arc::new((key, made.clone()))));
        Ok(made)
    }

    /// Whether it holds what's made of `key`.
    pub fn holds(&self, key: &K) -> bool {
        matches!(&*self.now.load(), Some(now) if now.0 == *key)
    }

    /// What it holds, when it's made of `key`: making nothing (goal 10, plan 01a1a87b2dae step 5: a read that
    /// finds it behind has it made off its shard). Counted as read, so its maker keeps it made.
    pub fn held(&self, key: &K) -> Option<Made> {
        self.note_read();
        match &*self.now.load() {
            Some(now) if now.0 == *key => Some(now.1.clone()),
            _ => None,
        }
    }

    /// Whether it's been read (its maker keeps only those made).
    pub fn asked(&self) -> bool {
        self.asked.load(Ordering::Relaxed)
    }

    /// What it holds now: what its maker makes over ([`Ahead::keep`]).
    pub fn now(&self) -> Option<Arc<(K, Made)>> {
        self.now.load_full()
    }

    /// What its maker made of `key`, in place of `was` (what it held when it began): unless a read
    /// made it meanwhile, which is as new.
    pub fn keep(&self, was: &Option<Arc<(K, Made)>>, key: K, made: Made) {
        let _ = self.now.compare_and_swap(was, Some(Arc::new((key, made))));
    }
}

/// The frames the server dropped toward each runner, by lane and reason (`crate::metrics::NetMetrics`).
pub type Drops = BTreeMap<u64, BTreeMap<(&'static str, &'static str), u64>>;

/// What `GET /api/agents` is made of (goal 10, plan 01a1a87b2dae step 2): the runners' rows as the
/// store holds them, their live records and processes, what's kept of what their wires refused and
/// the frames dropped toward them, each the copy it was read from, and how each connected one is
/// reached.
pub struct AgentsKey {
    pub rows: Arc<(u64, crate::store::AgentRows)>,
    pub live: Arc<HashMap<String, Arc<crate::hub::AgentLive>>>,
    pub instances: Arc<crate::instances::Instances>,
    pub refusals: Arc<crate::wire::unread::Refusals>,
    pub drops: Arc<Drops>,
    pub transports: Vec<(crate::instances::PeerId, crate::tunnel::Transport)>,
}

impl PartialEq for AgentsKey {
    fn eq(&self, o: &Self) -> bool {
        Arc::ptr_eq(&self.rows, &o.rows)
            && Arc::ptr_eq(&self.live, &o.live)
            && Arc::ptr_eq(&self.instances, &o.instances)
            && Arc::ptr_eq(&self.refusals, &o.refusals)
            && Arc::ptr_eq(&self.drops, &o.drops)
            && self.transports == o.transports
    }
}

/// What the server says of itself now, in `GET /api/system` and the overview.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemNow {
    /// Whole seconds: what the overview says changes once a second.
    pub uptime_s: u64,
    pub hostname: String,
    pub children: Vec<crate::sup::ChildStatus>,
    /// Its machine's newest sample.
    pub sample: Option<crate::metrics::HostSample>,
    pub kubernetes: bool,
}

/// What `GET /api/overview` is made of, for whoever reads every repository's runs (goal 10, plan
/// 01a1a87b2dae step 2): the last day's figures and the runners' rows as the store holds them, the
/// runners' live records, the server's own figures and the jobs queued and running.
pub struct OverviewKey {
    pub stats: Arc<(u64, crate::store::Stats)>,
    pub rows: Arc<(u64, crate::store::AgentRows)>,
    pub live: Arc<HashMap<String, Arc<crate::hub::AgentLive>>>,
    pub system: SystemNow,
    pub queue: (i64, i64),
}

impl PartialEq for OverviewKey {
    fn eq(&self, o: &Self) -> bool {
        Arc::ptr_eq(&self.stats, &o.stats) && Arc::ptr_eq(&self.rows, &o.rows) && Arc::ptr_eq(&self.live, &o.live) && self.system == o.system && self.queue == o.queue
    }
}

/// What the owner's `GET /api/me` says that can change (goal 10, plan 01a1a87b2dae step 2): where else
/// browsers find the server, what discovery announces, the host's name.
#[derive(Debug, PartialEq)]
pub struct MeKey {
    pub origins: Vec<String>,
    pub advert: Option<crate::discovery::Advert>,
    pub hostname: String,
}

/// A viewer's snapshot being written, counted in [`Views::attaching`] while it lives (goal 10, plan
/// 01a1a87b2dae step 3's trace).
pub struct Writing<'a>(&'a AtomicUsize);

impl<'a> Writing<'a> {
    pub fn of(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self(count)
    }
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// `parts`, each JSON encoded already, as one JSON array: joined, nothing encoded.
pub fn json_array<'a>(parts: impl Iterator<Item = &'a bytes::Bytes> + Clone) -> bytes::Bytes {
    let mut out = Vec::with_capacity(parts.clone().map(|p| p.len() + 1).sum::<usize>() + 2);
    out.push(b'[');
    for (i, p) in parts.enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(p);
    }
    out.push(b']');
    bytes::Bytes::from(out)
}

/// The reads kept in memory (goal 10, plan 01a1a87b2dae step 2): of the store's tables, made again when
/// they change ([`Kept`]: the plans' list, the agents' goal stacks, the runners' rows, the runs' list,
/// the last day's figures), and kept encoded ahead of their asking ([`Ahead`]: the runners, the
/// overview, the owner's `GET /api/me`).
pub struct Views {
    pub plans: Arc<Kept<crate::plans::Listed>>,
    pub goals: Arc<Kept<crate::goals::Stacks>>,
    pub agent_rows: Arc<Kept<crate::store::AgentRows>>,
    /// `GET /api/agents` for the owner.
    pub agents: Ahead<AgentsKey>,
    pub stats: Arc<Kept<crate::store::Stats>>,
    /// `GET /api/overview` for whoever reads every repository's runs.
    pub overview: Ahead<OverviewKey>,
    /// `GET /api/runs` with nothing in its query, for whoever reads every repository's runs.
    pub runs: Arc<Kept<bytes::Bytes>>,
    pub me: Ahead<MeKey>,
    /// Viewers whose snapshots are being written now ([`Writing`]; goal 10, plan 01a1a87b2dae step 3).
    pub attaching: AtomicUsize,
    /// Told when something a read kept ahead is made of changes: wakes their maker.
    wake: Arc<crate::rt::Notify>,
    started: AtomicBool,
}

impl Default for Views {
    fn default() -> Self {
        let wake = Arc::new(crate::rt::Notify::new());
        Self {
            plans: Arc::new(Kept::new(crate::plans::LISTED_FROM)),
            goals: Arc::new(Kept::new(crate::goals::STACKS_FROM)),
            agent_rows: Arc::new(Kept::telling(crate::store::AGENT_ROWS_FROM, &wake)),
            agents: Ahead::default(),
            stats: Arc::new(Kept::telling(crate::store::STATS_FROM, &wake)),
            overview: Ahead::telling(&wake),
            runs: Arc::new(Kept::new(RUNS_FROM)),
            me: Ahead::default(),
            attaching: AtomicUsize::new(0),
            wake,
            started: AtomicBool::new(false),
        }
    }
}

impl Views {
    /// Starts the maker of the reads kept ahead, once: a task of its own that makes each one read so
    /// far again as soon as something it's made of changes ([`Views::changed`]), and the overview as
    /// its uptime's next second comes while it's read, so a read finds it made. It holds the hub only
    /// while it makes them, and ends once the hub is gone.
    pub fn start(&self, hub: &Arc<crate::hub::Hub>) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        drop(crate::rt::spawn(make_ahead(Arc::downgrade(hub), Arc::clone(&self.wake))));
    }

    /// Something the reads kept ahead are made of changed (a runner's live record, the server's
    /// samples, the jobs' counts): their maker looks again.
    pub fn changed(&self) {
        self.wake.notify();
    }
}

impl Drop for Views {
    fn drop(&mut self) {
        // Their maker wakes to find the hub gone, and ends.
        self.wake.notify();
    }
}

/// The maker of the reads kept ahead ([`Views::start`]).
async fn make_ahead(hub: std::sync::Weak<crate::hub::Hub>, wake: Arc<crate::rt::Notify>) {
    loop {
        let next: Option<Instant> = match hub.upgrade() {
            Some(hub) => crate::api::make_ahead(&hub).await,
            None => return,
        };
        let woken = wake.notified();
        match next {
            Some(at) => {
                let _ = crate::rt::timeout(at.saturating_duration_since(Instant::now()), woken).await;
            }
            None => woken.await,
        }
    }
}

/// The runs a list shows when its query doesn't say how many.
const RUNS_LIMIT: u32 = 50;

/// The tables `GET /api/runs`' list is made of: the runs, their repositories, their jobs' hosts.
pub const RUNS_FROM: &[&str] = &["runs", "repos", "jobs"];

/// `GET /api/runs` as asked with nothing in its query (every repository's newest [`RUNS_LIMIT`], each
/// with its hosts: [`runs`]), as the store's tables hold it now, encoded once (goal 10, plan
/// 01a1a87b2dae step 2).
pub fn runs_listed(c: &rusqlite::Connection) -> anyhow::Result<bytes::Bytes> {
    let list = crate::store::runs_on(c, None, RUNS_LIMIT, None)?;
    let hosts = crate::store::run_hosts_on(c, &list.iter().map(|r| r.id.clone()).collect::<Vec<_>>())?;
    encode(&hosted(&list, &hosts)?)
}

/// Whether a replica answers `GET` of this route (its path's segments).
pub fn answers(segments: &[&str]) -> bool {
    matches!(
        segments,
        ["api", "runs"] | ["api", "runs", _] | ["api", "jobs", _] | ["api", "repos"] | ["api", "ci", "runs"] | ["api", "ci", "runs", _] | ["api", "ci", "runs", _, "graph"]
    )
}

/// Runs, newest first or by `sort` (oldest, longest, shortest), of the repositories `among` (all:
/// `None`), each with the runners and executors its legs ran on.
pub async fn runs(store: &Store, q: &HashMap<String, String>, among: Option<Vec<String>>) -> anyhow::Result<Vec<Value>> {
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(RUNS_LIMIT);
    let before = q.get("before").and_then(|v| v.parse().ok());
    let list = match q.get("sort").filter(|s| matches!(s.as_str(), "oldest" | "longest" | "shortest")) {
        // Another order than newest first pages by `offset` (W21-R03's sort).
        Some(sort) => store.runs_sorted(among, q.get("repo").cloned(), sort, limit, q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0)).await?,
        None => match among {
            None => store.runs(q.get("repo").cloned(), limit, before).await?,
            Some(names) => store.runs_among(names, q.get("repo").cloned(), limit, before).await?,
        },
    };
    with_hosts(store, &list).await
}

/// Runs, each with the runners and executors its legs ran on (W21-R03's facets).
pub async fn with_hosts(store: &Store, runs: &[Run]) -> anyhow::Result<Vec<Value>> {
    let hosts = store.run_hosts(runs.iter().map(|r| r.id.clone()).collect()).await?;
    hosted(runs, &hosts)
}

/// Runs, each with its runners and executors from `hosts` (by run).
fn hosted(runs: &[Run], hosts: &HashMap<String, (Vec<String>, Vec<String>)>) -> anyhow::Result<Vec<Value>> {
    let mut out = Vec::with_capacity(runs.len());
    for r in runs {
        let mut v = serde_json::to_value(r)?;
        if let (Some(o), Some((runners, executors))) = (v.as_object_mut(), hosts.get(&r.id)) {
            o.insert("runners".into(), json!(runners));
            o.insert("executors".into(), json!(executors));
        }
        out.push(v);
    }
    Ok(out)
}

/// A run and its jobs: a CI run's `attempt`, the current one by default (W05-R17). `None`: no such
/// run.
pub async fn run(store: &Store, id: &str, q: &HashMap<String, String>) -> anyhow::Result<Option<Value>> {
    let Some(run) = store.run(id.to_owned()).await? else { return Ok(None) };
    let attempt = q.get("attempt").and_then(|a| a.parse().ok());
    let jobs = store.jobs_at(id.to_owned(), false, attempt).await?;
    Ok(Some(json!({ "run": run, "jobs": jobs })))
}

/// A job. `None`: no such job.
pub async fn job(store: &Store, id: &str) -> anyhow::Result<Option<Value>> {
    Ok(store.job(id.to_owned()).await?.map(serde_json::to_value).transpose()?)
}

/// The repositories `keep` lets through, each saying whether it's public.
pub async fn repos(store: &Store, keep: impl Fn(&Repo) -> bool) -> anyhow::Result<Vec<Value>> {
    let mut repos = store.repos().await?;
    repos.retain(|r| keep(r));
    let public = store.public_repos().await.unwrap_or_default();
    Ok(repos
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.insert("public".into(), json!(public.contains(&r.id)));
            }
            v
        })
        .collect())
}

/// A replica's answer to the owner's `GET` of `segments` with query `q`, or `None` for anything it
/// doesn't answer as the server would (another route, nothing there, a read that failed): the
/// server answers that.
pub async fn answer(store: &Store, segments: &[&str], q: &HashMap<String, String>) -> Option<Value> {
    let read = match segments {
        ["api", "runs"] => runs(store, q, None).await.map(|v| Some(Value::Array(v))),
        ["api", "runs", id] => run(store, id, q).await,
        ["api", "jobs", id] => job(store, id).await,
        ["api", "repos"] => repos(store, |_| true).await.map(|v| Some(Value::Array(v))),
        // The CI pages' own (the engine's runs, a run with its legs, its graph).
        ["api", "ci", "runs"] => return crate::ci::runs_api::list_in(store, q, None).await.ok(),
        ["api", "ci", "runs", id] => return crate::ci::runs_api::get_in(store, id, q).await.ok(),
        ["api", "ci", "runs", id, "graph"] => return crate::ci::runs_api::graph_in(store, id, q).await.ok(),
        _ => return None,
    };
    match read {
        Ok(v) => v,
        Err(e) => {
            // A replica whose schema is another build's, say: the server answers.
            tracing::debug!(error = %format!("{e:#}"), "a read the replica couldn't answer");
            None
        }
    }
}
