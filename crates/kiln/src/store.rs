//! Durable state in SQLite (WAL). Every call runs on the blocking pool so the async runtime is
//! never stalled by disk I/O. Logs are *not* stored here (see `logs.rs`).

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::pipeline::PlannedJob;

pub mod capture;
pub mod group;
pub mod migrations;

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

/// The store's connection, and with the cluster group what its writes go through.
struct Inner {
    // Dropped in order: the connection before the watch its hook writes into.
    conn: Mutex<Connection>,
    watch: Option<capture::Watch>,
    group: Option<group::Group>,
    /// A folder of its own, removed with the store (tests through the group, [`Store::in_memory`]).
    scratch: Option<std::path::PathBuf>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(dir) = self.scratch.take() {
            // The group's thread and connections go first: then the folder.
            self.group.take();
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Tables this node keeps for itself: its log search index, built from its own logs. Written with
/// [`Store::call_local`], past the cluster group; a write through the group must not touch them.
pub const LOCAL: &[&str] = &["ci_log_indexed", "ci_log_index_data", "ci_log_index_idx", "ci_log_index_content", "ci_log_index_docsize", "ci_log_index_config"];

/// A transaction that nests: a SQLite savepoint. At the top it opens one as `BEGIN DEFERRED`
/// would; inside another (the one a write runs in while the cluster group takes it, docs/p2p.md
/// P1) it becomes part of that. [`Tx::commit`] keeps what it did; dropped without, it's undone.
pub struct Tx<'c> {
    conn: &'c Connection,
    open: bool,
}

/// Opens a [`Tx`] on `conn`: what every write that needs its statements together uses, rather than
/// rusqlite's transactions, which can't be opened inside another.
pub fn tx(conn: &Connection) -> rusqlite::Result<Tx<'_>> {
    conn.execute_batch("SAVEPOINT kiln_tx")?;
    Ok(Tx { conn, open: true })
}

impl Tx<'_> {
    /// Keeps what it did: commits it at the top, makes it part of the one around it inside one.
    pub fn commit(mut self) -> rusqlite::Result<()> {
        self.open = false;
        self.conn.execute_batch("RELEASE kiln_tx")
    }
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if self.open {
            let _ = self.conn.execute_batch("ROLLBACK TO kiln_tx; RELEASE kiln_tx");
        }
    }
}

impl std::ops::Deref for Tx<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
    }
}

/// Runs a write through the cluster group (see [`Store::call`]).
fn through_group<R>(conn: &mut Connection, watch: &capture::Watch, group: &group::Group, f: impl FnOnce(&mut Connection) -> anyhow::Result<R>) -> anyhow::Result<R> {
    // IMMEDIATE: the write lock now, so what `f` reads is what it writes over (a transaction that
    // read first would be refused its write had the group's thread committed meanwhile).
    conn.execute_batch("BEGIN IMMEDIATE")?;
    watch.begin();
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(conn)));
    let change = watch.end(conn);
    let result = match ran {
        Ok(r) => r,
        Err(panic) => {
            conn.execute_batch("ROLLBACK").ok();
            std::panic::resume_unwind(panic)
        }
    };
    let change = match change {
        Ok(c) => c,
        Err(e) => {
            // What it wrote couldn't be read for the log: it isn't kept.
            conn.execute_batch("ROLLBACK").ok();
            return Err(e);
        }
    };
    if change.is_empty() {
        conn.execute_batch("ROLLBACK")?;
        return result;
    }
    // Kept as without the group (an `f` that failed after writing keeps what it wrote), with its
    // number; then the change goes to the log.
    let seq = group.next_seq();
    let kept = conn
        .execute(&format!("UPDATE {} SET seq = ?1 WHERE one = 1", group::STATE), [i64::try_from(seq)?])
        .map_err(anyhow::Error::from)
        .and_then(|_| conn.execute_batch("COMMIT").map_err(anyhow::Error::from));
    if let Err(e) = kept {
        conn.execute_batch("ROLLBACK").ok();
        return Err(e);
    }
    group.append(seq, change);
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
    TimedOut,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "skipped" => Self::Skipped,
            "timed_out" => Self::TimedOut,
            _ => Self::Pending,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Skipped | Self::TimedOut
        )
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Succeeded | Self::Skipped)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    /// `owner/name`.
    pub name: String,
    pub clone_url: String,
    pub branches: Vec<String>,
    pub poll_seconds: u32,
    pub enabled: bool,
    pub created_at: i64,
    pub last_polled_at: Option<i64>,
    pub last_error: Option<String>,
    /// `upstream` (kiln follows `clone_url`) or `native` (kiln hosts it, docs/ci.md W14-R01).
    #[serde(default = "upstream")]
    pub mode: String,
    /// A native repository's trunk when it was made.
    #[serde(default)]
    pub default_branch: Option<String>,
    /// A native repository's upstream, its trunk pushed there after each land (W14-R01).
    #[serde(default)]
    pub mirror: Option<String>,
}

fn upstream() -> String {
    "upstream".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub repo_id: String,
    pub repo_name: String,
    pub number: i64,
    pub pipeline: String,
    pub event: String,
    pub branch: String,
    pub sha: String,
    pub message: String,
    pub author: String,
    pub status: Status,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    pub concurrency_group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub run_id: String,
    pub key: String,
    pub display: String,
    pub status: Status,
    pub labels: Vec<String>,
    pub needs: Vec<String>,
    pub agent: Option<String>,
    pub executor: Option<String>,
    pub attempt: u32,
    pub queued_at: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub peak_rss: Option<u64>,
    pub cpu_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<PlannedJob>,
    pub steps: Vec<Step>,
    /// A CI matrix leg's values, by axis: where it sits in its job's grid on the run graph
    /// (docs/ci.md W21-R04).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub idx: u32,
    pub name: String,
    pub status: Status,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    /// The order it started in among the job's steps (CI legs; docs/ci.md W21).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRow {
    pub name: String,
    pub labels: Vec<String>,
    pub slots: u32,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub ephemeral: bool,
    pub connected: bool,
    pub last_seen: i64,
    pub enrolled_at: i64,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
CREATE TABLE IF NOT EXISTS repos (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, clone_url TEXT NOT NULL,
  branches TEXT NOT NULL DEFAULT '[]', poll_seconds INTEGER NOT NULL DEFAULT 60,
  enabled INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL,
  last_polled_at INTEGER, last_error TEXT, etag TEXT
);
CREATE TABLE IF NOT EXISTS refs (
  repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  name TEXT NOT NULL, sha TEXT NOT NULL, PRIMARY KEY (repo_id, name)
);
CREATE TABLE IF NOT EXISTS runs (
  id TEXT PRIMARY KEY, repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  number INTEGER NOT NULL, pipeline TEXT NOT NULL, event TEXT NOT NULL, branch TEXT NOT NULL,
  sha TEXT NOT NULL, message TEXT NOT NULL DEFAULT '', author TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL, created_at INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER,
  error TEXT, concurrency_group TEXT, UNIQUE (repo_id, number)
);
CREATE INDEX IF NOT EXISTS runs_created ON runs (created_at DESC);
CREATE INDEX IF NOT EXISTS runs_status ON runs (status);
CREATE TABLE IF NOT EXISTS jobs (
  id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  key TEXT NOT NULL, display TEXT NOT NULL, status TEXT NOT NULL, labels TEXT NOT NULL,
  needs TEXT NOT NULL, spec TEXT NOT NULL, agent TEXT, executor TEXT,
  attempt INTEGER NOT NULL DEFAULT 0, queued_at INTEGER, started_at INTEGER, finished_at INTEGER,
  exit_code INTEGER, error TEXT, peak_rss INTEGER, cpu_seconds REAL
);
CREATE INDEX IF NOT EXISTS jobs_run ON jobs (run_id);
CREATE INDEX IF NOT EXISTS jobs_status ON jobs (status);
CREATE TABLE IF NOT EXISTS steps (
  job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE, idx INTEGER NOT NULL,
  name TEXT NOT NULL, status TEXT NOT NULL, started_at INTEGER, finished_at INTEGER,
  exit_code INTEGER, PRIMARY KEY (job_id, idx)
);
CREATE TABLE IF NOT EXISTS agents (
  name TEXT PRIMARY KEY, labels TEXT NOT NULL, slots INTEGER NOT NULL, hostname TEXT NOT NULL,
  os TEXT NOT NULL, arch TEXT NOT NULL, version TEXT NOT NULL, ephemeral INTEGER NOT NULL,
  connected INTEGER NOT NULL DEFAULT 0, last_seen INTEGER NOT NULL, enrolled_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, actor TEXT NOT NULL,
  action TEXT NOT NULL, detail TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
"#;

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_owned())
}

fn from_json<T: for<'de> Deserialize<'de> + Default>(s: &str) -> T {
    serde_json::from_str(s).unwrap_or_default()
}

impl Store {
    /// The store at `path`, written to directly (no cluster group): tools that run with the server
    /// stopped, and setups that turn the group off (`[cluster] group = false`).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Self::connect(path)?;
        Ok(Self { inner: Arc::new(Inner { conn: Mutex::new(conn), watch: None, group: None, scratch: None }) })
    }

    /// The store at `path` with its writes through the cluster group, its log in `cluster`
    /// (docs/p2p.md, P1). What the log holds past the database is replayed into it first.
    pub fn open_grouped(path: &Path, cluster: &Path) -> anyhow::Result<Self> {
        let conn = Self::connect(path)?;
        let group = group::Group::open(path, cluster).context("the cluster group")?;
        let watch = capture::Watch::install(&conn, LOCAL)?;
        Ok(Self { inner: Arc::new(Inner { conn: Mutex::new(conn), watch: Some(watch), group: Some(group), scratch: None }) })
    }

    fn connect(path: &Path) -> anyhow::Result<Connection> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("apply schema")?;
        migrations::apply(&mut conn).context("apply migrations")?;
        Ok(conn)
    }

    /// A store of its own for tests: in memory, or with `KILN_STORE_GROUP=1` a database in a scratch
    /// folder with its writes through the cluster group, so the whole suite can run that way.
    pub fn in_memory() -> anyhow::Result<Self> {
        if std::env::var("KILN_STORE_GROUP").is_ok_and(|v| v == "1") {
            let dir = std::env::temp_dir().join(format!("kiln-store-{}", crate::util::new_id()));
            let mut store = Self::open_grouped(&dir.join("kiln.db"), &dir.join("cluster"))?;
            if let Some(inner) = Arc::get_mut(&mut store.inner) {
                inner.scratch = Some(dir);
            }
            return Ok(store);
        }
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrations::apply(&mut conn)?;
        Ok(Self { inner: Arc::new(Inner { conn: Mutex::new(conn), watch: None, group: None, scratch: None }) })
    }

    /// Whether this store's writes go through the cluster group.
    pub fn grouped(&self) -> bool {
        self.inner.group.is_some()
    }

    /// With the cluster group, where it stands: kiln.db's last write and the last entry applied.
    pub fn group_standing(&self) -> Option<(u64, u64)> {
        self.inner.group.as_ref().map(group::Group::standing)
    }

    /// Waits until every write so far is in the cluster group's log (tests; a clean stop).
    pub fn flush_group(&self) -> anyhow::Result<()> {
        match &self.inner.group {
            Some(g) => g.flush(),
            None => Ok(()),
        }
    }

    /// Runs `f` with the connection on the blocking pool. With the cluster group, what `f` writes
    /// goes through it: `f` runs in a transaction while the rows it touches are noted, which
    /// commits with the write's number and sends the rows' state (its change) to the group's log,
    /// which a restart replays. A read is only a read.
    pub async fn call<R, F>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let inner = self.inner.clone();
        crate::rt::blocking(move || {
            let mut g = inner.conn.lock();
            match (&inner.watch, &inner.group) {
                (Some(watch), Some(group)) => through_group(&mut g, watch, group, f),
                _ => f(&mut g),
            }
        })
        .await?
    }

    /// Runs `f` with the connection, past the cluster group: for this node's own tables ([`LOCAL`]).
    pub async fn call_local<R, F>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let inner = self.inner.clone();
        crate::rt::blocking(move || {
            let mut g = inner.conn.lock();
            f(&mut g)
        })
        .await?
    }

    /// Runners connected just before this server started: those still marked connected (a server
    /// that was killed didn't mark them gone), or gone within `within_s` (its shutdown, a restart).
    /// Not a job's own pod, which doesn't come back. Read before [`Store::recover`]; how many
    /// return is a deploy's check (`/healthz/server`).
    pub async fn runners_before(&self, within_s: i64) -> anyhow::Result<u64> {
        let since = crate::util::now_unix() - within_s;
        self.call(move |c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM agents WHERE ephemeral = 0 AND (connected = 1 OR last_seen >= ?1)", [since], |r| r.get(0))?;
            Ok(u64::try_from(n).unwrap_or(0))
        })
        .await
    }

    /// After a server restart every agent is marked disconnected until it reconnects. Running jobs
    /// are left alone: agents keep executing through a server outage and reclaim their jobs in
    /// `Hello`; whatever is not reclaimed within the grace period is requeued by the scheduler.
    /// Returns how many jobs are awaiting reclamation.
    pub async fn recover(&self) -> anyhow::Result<usize> {
        self.call(|c| {
            c.execute("UPDATE agents SET connected = 0", [])?;
            let n: i64 = c.query_row("SELECT COUNT(*) FROM jobs WHERE status = 'running'", [], |r| r.get(0))?;
            Ok(usize::try_from(n).unwrap_or(0))
        })
        .await
    }

    /// Put a claimed job back in the queue *without* spending an attempt (the agent declined it
    /// before doing any work).
    pub async fn unclaim_job(&self, id: String, note: String) -> anyhow::Result<()> {
        self.call(move |c| {
            let tx = crate::store::tx(c)?;
            tx.execute(
                "UPDATE jobs SET status = 'queued', agent = NULL, executor = NULL, started_at = NULL,
                   error = ?2 WHERE id = ?1 AND status = 'running'",
                params![id, note],
            )?;
            tx.execute(
                "UPDATE steps SET status = 'pending', started_at = NULL, finished_at = NULL,
                   exit_code = NULL WHERE job_id = ?1",
                [&id],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    // ---------------------------------------------------------------- repos

    /// Add a repository, or update it by name. One that moved to another remote forgets the
    /// branches and tags it saw at the old one: its next poll adopts the new remote's as they are,
    /// rather than building every ref that differs between the two.
    pub async fn upsert_repo(&self, r: Repo) -> anyhow::Result<Repo> {
        self.call(move |c| {
            let before: Option<(String, String)> = c
                .query_row("SELECT id, clone_url FROM repos WHERE name = ?1", [&r.name], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            if let Some((id, url)) = &before
                && url != &r.clone_url
            {
                c.execute("DELETE FROM refs WHERE repo_id = ?1", [id])?;
                tracing::info!(repo = %r.name, from = %url, to = %r.clone_url, "repository moved: adopting its new remote's branches and tags as they are");
            }
            c.execute(
                "INSERT INTO repos (id, name, clone_url, branches, poll_seconds, enabled, created_at, mode, default_branch, mirror)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(name) DO UPDATE SET clone_url = excluded.clone_url,
                   branches = excluded.branches, poll_seconds = excluded.poll_seconds,
                   enabled = excluded.enabled, mode = excluded.mode,
                   default_branch = COALESCE(excluded.default_branch, repos.default_branch), mirror = excluded.mirror",
                params![
                    r.id,
                    r.name,
                    r.clone_url,
                    json(&r.branches),
                    r.poll_seconds,
                    r.enabled,
                    r.created_at,
                    r.mode,
                    r.default_branch,
                    r.mirror
                ],
            )?;
            let id: String =
                c.query_row("SELECT id FROM repos WHERE name = ?1", [&r.name], |row| row.get(0))?;
            Ok(Repo { id, ..r })
        })
        .await
    }

    /// Whether a native repository is public: anyone may read it over git (W14-R01).
    pub async fn repo_public(&self, id: String) -> anyhow::Result<bool> {
        self.call(move |c| Ok(c.query_row("SELECT public FROM repos WHERE id = ?1", [&id], |r| r.get::<_, bool>(0)).optional()?.unwrap_or(false))).await
    }

    /// The public repositories' ids.
    pub async fn public_repos(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        self.call(|c| {
            let mut st = c.prepare("SELECT id FROM repos WHERE public = 1")?;
            let ids = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?;
            Ok(ids)
        })
        .await
    }

    pub async fn set_repo_public(&self, id: String, public: bool) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute("UPDATE repos SET public = ?2 WHERE id = ?1", params![id, public])?;
            Ok(())
        })
        .await
    }

    pub async fn delete_repo(&self, id: String) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute("DELETE FROM repos WHERE id = ?1 OR name = ?1", [id])?;
            Ok(())
        })
        .await
    }

    pub async fn repos(&self) -> anyhow::Result<Vec<Repo>> {
        self.call(|c| {
            let mut st = c.prepare(
                "SELECT id, name, clone_url, branches, poll_seconds, enabled, created_at,
                        last_polled_at, last_error, mode, default_branch, mirror FROM repos ORDER BY name",
            )?;
            let rows = st.query_map([], |r| {
                Ok(Repo {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    clone_url: r.get(2)?,
                    branches: from_json(&r.get::<_, String>(3)?),
                    poll_seconds: r.get(4)?,
                    enabled: r.get(5)?,
                    created_at: r.get(6)?,
                    last_polled_at: r.get(7)?,
                    last_error: r.get(8)?,
                    mode: r.get(9)?,
                    default_branch: r.get(10)?,
                    mirror: r.get(11)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    pub async fn repo_polled(&self, id: String, error: Option<String>) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute(
                "UPDATE repos SET last_polled_at = ?2, last_error = ?3 WHERE id = ?1",
                params![id, crate::util::now_unix(), error],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn repo_etag(&self, id: String) -> anyhow::Result<Option<String>> {
        self.call(move |c| {
            Ok(c.query_row("SELECT etag FROM repos WHERE id = ?1", [id], |r| r.get(0))
                .optional()?
                .flatten())
        })
        .await
    }

    pub async fn set_repo_etag(&self, id: String, etag: Option<String>) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute("UPDATE repos SET etag = ?2 WHERE id = ?1", params![id, etag])?;
            Ok(())
        })
        .await
    }

    pub async fn refs(&self, repo_id: String) -> anyhow::Result<Vec<(String, String)>> {
        self.call(move |c| {
            let mut st = c.prepare("SELECT name, sha FROM refs WHERE repo_id = ?1")?;
            let rows = st.query_map([repo_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Records that `name` is now at `sha`, unless it already was: whether this call moved it. The
    /// sources that watch refs (polls, webhooks) call this, so a move triggers once.
    pub async fn advance_ref(&self, repo_id: String, name: String, sha: String) -> anyhow::Result<bool> {
        self.call(move |c| {
            let n = c.execute(
                "INSERT INTO refs (repo_id, name, sha) VALUES (?1, ?2, ?3)
                 ON CONFLICT(repo_id, name) DO UPDATE SET sha = excluded.sha WHERE refs.sha IS NOT excluded.sha",
                params![repo_id, name, sha],
            )?;
            Ok(n > 0)
        })
        .await
    }

    pub async fn set_ref(&self, repo_id: String, name: String, sha: String) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO refs (repo_id, name, sha) VALUES (?1, ?2, ?3)
                 ON CONFLICT(repo_id, name) DO UPDATE SET sha = excluded.sha",
                params![repo_id, name, sha],
            )?;
            Ok(())
        })
        .await
    }

    // ---------------------------------------------------------------- runs

    /// Insert a run with its planned jobs atomically. Jobs with no `needs` start queued.
    pub async fn create_run(&self, mut run: Run, jobs: Vec<PlannedJob>) -> anyhow::Result<Run> {
        self.call(move |c| {
            let tx = crate::store::tx(c)?;
            let number: i64 = tx.query_row(
                "SELECT COALESCE(MAX(number), 0) + 1 FROM runs WHERE repo_id = ?1",
                [&run.repo_id],
                |r| r.get(0),
            )?;
            run.number = number;
            let now = crate::util::now_unix();
            run.status = if jobs.is_empty() { Status::Skipped } else { Status::Queued };
            tx.execute(
                "INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, message,
                   author, status, created_at, error, concurrency_group, finished_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    run.id,
                    run.repo_id,
                    number,
                    run.pipeline,
                    run.event,
                    run.branch,
                    run.sha,
                    crate::util::truncate(&run.message, 4000),
                    run.author,
                    run.status.as_str(),
                    now,
                    run.error,
                    run.concurrency_group,
                    if jobs.is_empty() { Some(now) } else { None },
                ],
            )?;
            for j in &jobs {
                let status = if j.needs.is_empty() { Status::Queued } else { Status::Pending };
                let id = crate::util::new_id();
                tx.execute(
                    "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, queued_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        id,
                        run.id,
                        j.key,
                        j.display,
                        status.as_str(),
                        json(&j.labels),
                        json(&j.needs),
                        json(j),
                        if status == Status::Queued { Some(now) } else { None },
                    ],
                )?;
                for (i, s) in j.steps.iter().enumerate() {
                    let name = s.name.clone().unwrap_or_else(|| {
                        crate::util::truncate(s.run.lines().next().unwrap_or("step"), 80).to_owned()
                    });
                    tx.execute(
                        "INSERT INTO steps (job_id, idx, name, status) VALUES (?1, ?2, ?3, 'pending')",
                        params![id, i64::try_from(i).unwrap_or(i64::MAX), name],
                    )?;
                }
            }
            tx.commit()?;
            run.created_at = now;
            Ok(run)
        })
        .await
    }

    pub async fn runs(&self, repo: Option<String>, limit: u32, before: Option<i64>) -> anyhow::Result<Vec<Run>> {
        self.call(move |c| {
            let mut st = c.prepare(
                "SELECT r.id, r.repo_id, p.name, r.number, r.pipeline, r.event, r.branch, r.sha,
                        r.message, r.author, r.status, r.created_at, r.started_at, r.finished_at,
                        r.error, r.concurrency_group
                 FROM runs r JOIN repos p ON p.id = r.repo_id
                 WHERE (?1 IS NULL OR p.id = ?1 OR p.name = ?1)
                   AND (?3 IS NULL OR r.created_at < ?3)
                 ORDER BY r.created_at DESC, r.number DESC LIMIT ?2",
            )?;
            let rows = st.query_map(params![repo, limit.min(500), before], row_run)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// [`Self::runs`], of the repositories named only.
    pub async fn runs_among(&self, names: Vec<String>, repo: Option<String>, limit: u32, before: Option<i64>) -> anyhow::Result<Vec<Run>> {
        let names = serde_json::to_string(&names)?;
        self.call(move |c| {
            let mut st = c.prepare(
                "SELECT r.id, r.repo_id, p.name, r.number, r.pipeline, r.event, r.branch, r.sha,
                        r.message, r.author, r.status, r.created_at, r.started_at, r.finished_at,
                        r.error, r.concurrency_group
                 FROM runs r JOIN repos p ON p.id = r.repo_id
                 WHERE (?1 IS NULL OR p.id = ?1 OR p.name = ?1)
                   AND (?3 IS NULL OR r.created_at < ?3)
                   AND p.name IN (SELECT value FROM json_each(?4))
                 ORDER BY r.created_at DESC, r.number DESC LIMIT ?2",
            )?;
            let rows = st.query_map(params![repo, limit.min(500), before, names], row_run)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Runs in another order than newest first (the runs list's sort, W21-R03): `oldest`, or
    /// `longest` and `shortest` (a running one's time so far), from `offset` on; `among` the
    /// repositories a person may read, when narrowed.
    /// The runners and executors each of `ids`'s legs ran on (W21-R03's facets), by run.
    pub async fn run_hosts(&self, ids: Vec<String>) -> anyhow::Result<std::collections::HashMap<String, (Vec<String>, Vec<String>)>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        self.call(move |c| {
            let mut st = c.prepare("SELECT run_id, agent, executor FROM jobs WHERE run_id IN (SELECT value FROM json_each(?1)) AND (agent IS NOT NULL OR executor IS NOT NULL) ORDER BY run_id")?;
            let rows = st.query_map([serde_json::to_string(&ids)?], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)))?;
            let mut out: std::collections::HashMap<String, (Vec<String>, Vec<String>)> = std::collections::HashMap::new();
            for row in rows {
                let (run, agent, executor) = row?;
                let hosts = out.entry(run).or_default();
                for (list, v) in [(&mut hosts.0, agent), (&mut hosts.1, executor)] {
                    if let Some(v) = v.filter(|v| !v.is_empty())
                        && !list.contains(&v)
                    {
                        list.push(v);
                    }
                }
            }
            Ok(out)
        })
        .await
    }

    pub async fn runs_sorted(&self, among: Option<Vec<String>>, repo: Option<String>, order: &str, limit: u32, offset: u32) -> anyhow::Result<Vec<Run>> {
        const TOOK: &str = "(COALESCE(r.finished_at, CAST(strftime('%s', 'now') AS INTEGER)) - COALESCE(r.started_at, r.created_at))";
        let by = match order {
            "oldest" => "r.created_at ASC, r.number ASC".to_owned(),
            "longest" => format!("{TOOK} DESC, r.created_at DESC"),
            "shortest" => format!("{TOOK} ASC, r.created_at DESC"),
            _ => "r.created_at DESC, r.number DESC".to_owned(),
        };
        let names = among.map(|n| serde_json::to_string(&n).unwrap_or_default());
        self.call(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT r.id, r.repo_id, p.name, r.number, r.pipeline, r.event, r.branch, r.sha,
                        r.message, r.author, r.status, r.created_at, r.started_at, r.finished_at,
                        r.error, r.concurrency_group
                 FROM runs r JOIN repos p ON p.id = r.repo_id
                 WHERE (?1 IS NULL OR p.id = ?1 OR p.name = ?1)
                   AND (?4 IS NULL OR p.name IN (SELECT value FROM json_each(?4)))
                 ORDER BY {by} LIMIT ?2 OFFSET ?3"
            ))?;
            let rows = st.query_map(params![repo, limit.min(500), offset, names], row_run)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    pub async fn run(&self, id: String) -> anyhow::Result<Option<Run>> {
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT r.id, r.repo_id, p.name, r.number, r.pipeline, r.event, r.branch, r.sha,
                        r.message, r.author, r.status, r.created_at, r.started_at, r.finished_at,
                        r.error, r.concurrency_group
                 FROM runs r JOIN repos p ON p.id = r.repo_id WHERE r.id = ?1",
                [id],
                row_run,
            )
            .optional()?)
        })
        .await
    }

    pub async fn jobs(&self, run_id: String, with_spec: bool) -> anyhow::Result<Vec<Job>> {
        self.jobs_at(run_id, with_spec, None).await
    }

    /// The leg a CI rerun's kept leg keeps the result of (docs/ci.md W05-R17).
    pub async fn reused_from(&self, job: String) -> anyhow::Result<Option<String>> {
        self.call(move |c| Ok(c.query_row("SELECT reused_from FROM jobs WHERE id = ?1", [&job], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())).await
    }

    /// A run's jobs; for a CI run, the legs of `attempt` (the current one when `None`, docs/ci.md
    /// W05-R17). v1's jobs have no job_id and no attempts.
    pub async fn jobs_at(&self, run_id: String, with_spec: bool, attempt: Option<i64>) -> anyhow::Result<Vec<Job>> {
        self.call(move |c| {
            let mut st = c.prepare(&format!("{JOB_SELECT} WHERE run_id = ?1 AND (job_id IS NULL OR run_attempt = COALESCE(?2, (SELECT attempt FROM runs WHERE id = ?1))) ORDER BY rowid"))?;
            let mut jobs: Vec<Job> =
                st.query_map(params![run_id, attempt], row_job)?.collect::<Result<Vec<_>, _>>()?;
            for j in &mut jobs {
                j.steps = steps_of(c, &j.id)?;
                if !with_spec {
                    j.spec = None;
                }
            }
            Ok(jobs)
        })
        .await
    }

    pub async fn job(&self, id: String) -> anyhow::Result<Option<Job>> {
        self.call(move |c| {
            let j = c.query_row(&format!("{JOB_SELECT} WHERE id = ?1"), [&id], row_job).optional()?;
            match j {
                Some(mut j) => {
                    j.steps = steps_of(c, &j.id)?;
                    Ok(Some(j))
                }
                None => Ok(None),
            }
        })
        .await
    }

    /// v1 jobs waiting for an agent, oldest first (legs of the CI engine, with a `job_id`, are its
    /// dispatcher's, docs/ci.md §2.4). A run of a concurrency group waits while an older run of the
    /// group is active: one of a group runs at a time.
    pub async fn queued_jobs(&self, limit: u32) -> anyhow::Result<Vec<Job>> {
        self.call(move |c| {
            let mut st = c.prepare(&format!(
                "{JOB_SELECT} WHERE status = 'queued' AND job_id IS NULL AND NOT EXISTS (
                   SELECT 1 FROM runs r JOIN runs o ON o.concurrency_group = r.concurrency_group
                   WHERE r.id = jobs.run_id AND o.id != r.id AND o.status IN ('queued','running','pending')
                     AND (o.created_at < r.created_at OR (o.created_at = r.created_at AND o.rowid < r.rowid)))
                 ORDER BY queued_at, rowid LIMIT ?1"
            ))?;
            Ok(st.query_map([limit], row_job)?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// v1 jobs running (the CI engine's legs are its dispatcher's).
    pub async fn running_jobs(&self) -> anyhow::Result<Vec<Job>> {
        self.call(|c| {
            let mut st = c.prepare(&format!("{JOB_SELECT} WHERE status = 'running' AND job_id IS NULL"))?;
            Ok(st.query_map([], row_job)?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Which of `ids` are the CI engine's legs (rows of a workflow's job), not v1's jobs.
    pub async fn ci_legs(&self, ids: Vec<String>) -> anyhow::Result<std::collections::HashSet<String>> {
        if ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        self.call(move |c| {
            let mut st = c.prepare("SELECT 1 FROM jobs WHERE id = ?1 AND job_id IS NOT NULL")?;
            let mut out = std::collections::HashSet::new();
            for id in ids {
                if st.exists([&id])? {
                    out.insert(id);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Atomically claim a queued job for an agent. Returns false if someone else got it.
    pub async fn claim_job(&self, id: String, agent: String, executor: String) -> anyhow::Result<bool> {
        self.call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            let n = tx.execute(
                "UPDATE jobs SET status = 'running', agent = ?2, executor = ?3, started_at = ?4,
                   finished_at = NULL, exit_code = NULL, error = NULL
                 WHERE id = ?1 AND status = 'queued'",
                params![id, agent, executor, now],
            )?;
            if n == 1 {
                tx.execute(
                    "UPDATE runs SET status = 'running', started_at = COALESCE(started_at, ?2)
                     WHERE id = (SELECT run_id FROM jobs WHERE id = ?1)",
                    params![id, now],
                )?;
            }
            tx.commit()?;
            Ok(n == 1)
        })
        .await
    }

    pub async fn step_update(
        &self,
        job_id: String,
        idx: u32,
        status: Status,
        exit_code: Option<i32>,
    ) -> anyhow::Result<()> {
        self.call(move |c| {
            let now = crate::util::now_unix();
            if status == Status::Running {
                c.execute(
                    "UPDATE steps SET status = ?3, started_at = ?4 WHERE job_id = ?1 AND idx = ?2",
                    params![job_id, idx, status.as_str(), now],
                )?;
            } else {
                c.execute(
                    "UPDATE steps SET status = ?3, finished_at = ?4, exit_code = ?5,
                       started_at = COALESCE(started_at, ?4)
                     WHERE job_id = ?1 AND idx = ?2",
                    params![job_id, idx, status.as_str(), now, exit_code],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Finish a job, then advance its run: unblock dependants, skip the ones whose dependency
    /// failed, and settle the run status once everything is terminal. Returns the run id and the
    /// run's new status if the run finished.
    pub async fn finish_job(
        &self,
        id: String,
        status: Status,
        exit_code: Option<i32>,
        error: Option<String>,
        peak_rss: Option<u64>,
        cpu_seconds: Option<f64>,
    ) -> anyhow::Result<(String, Option<Status>)> {
        self.call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            tx.execute(
                "UPDATE jobs SET status = ?2, finished_at = ?3, exit_code = ?4, error = ?5,
                   peak_rss = ?6, cpu_seconds = ?7 WHERE id = ?1",
                params![
                    id,
                    status.as_str(),
                    now,
                    exit_code,
                    error,
                    peak_rss.and_then(|v| i64::try_from(v).ok()),
                    cpu_seconds
                ],
            )?;
            tx.execute(
                "UPDATE steps SET status = 'skipped' WHERE job_id = ?1 AND status IN ('pending','running')",
                [&id],
            )?;
            let run_id: String =
                tx.query_row("SELECT run_id FROM jobs WHERE id = ?1", [&id], |r| r.get(0))?;
            let settled = advance_run(&tx, &run_id, now)?;
            tx.commit()?;
            Ok((run_id, settled))
        })
        .await
    }

    /// Put a running job back in the queue (agent lost) or fail it when out of retries.
    pub async fn requeue_job(&self, id: String, reason: String) -> anyhow::Result<bool> {
        self.call(move |c| {
            let tx = crate::store::tx(c)?;
            let (attempt, spec): (u32, String) = tx.query_row(
                "SELECT attempt, spec FROM jobs WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let retries = serde_json::from_str::<PlannedJob>(&spec).map(|s| s.retries).unwrap_or(0);
            let now = crate::util::now_unix();
            let requeued = attempt < retries.max(2);
            if requeued {
                tx.execute(
                    "UPDATE jobs SET status = 'queued', agent = NULL, attempt = attempt + 1,
                       queued_at = ?2, error = ?3 WHERE id = ?1 AND status = 'running'",
                    params![id, now, reason],
                )?;
                tx.execute(
                    "UPDATE steps SET status = 'pending', started_at = NULL, finished_at = NULL,
                       exit_code = NULL WHERE job_id = ?1",
                    [&id],
                )?;
            } else {
                tx.execute(
                    "UPDATE jobs SET status = 'failed', finished_at = ?2, error = ?3 WHERE id = ?1",
                    params![id, now, reason],
                )?;
                let run_id: String =
                    tx.query_row("SELECT run_id FROM jobs WHERE id = ?1", [&id], |r| r.get(0))?;
                advance_run(&tx, &run_id, now)?;
            }
            tx.commit()?;
            Ok(requeued)
        })
        .await
    }

    /// Cancel every non-terminal job of a run. Returns the ids of jobs that were running so the
    /// caller can tell their agents to stop.
    pub async fn cancel_run(&self, run_id: String, reason: String) -> anyhow::Result<Vec<(String, Option<String>)>> {
        self.call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            let running: Vec<(String, Option<String>)> = {
                let mut st = tx.prepare(
                    "SELECT id, agent FROM jobs WHERE run_id = ?1 AND status = 'running'",
                )?;
                st.query_map([&run_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            tx.execute(
                "UPDATE jobs SET status = 'cancelled', finished_at = ?2, error = ?3
                 WHERE run_id = ?1 AND status IN ('pending','queued','running')",
                params![run_id, now, reason],
            )?;
            tx.execute(
                "UPDATE steps SET status = 'cancelled' WHERE status IN ('pending','running')
                   AND job_id IN (SELECT id FROM jobs WHERE run_id = ?1)",
                [&run_id],
            )?;
            tx.execute(
                "UPDATE runs SET status = 'cancelled', finished_at = ?2, error = COALESCE(error, ?3)
                 WHERE id = ?1 AND status NOT IN ('succeeded','failed','cancelled','skipped','timed_out')",
                params![run_id, now, reason],
            )?;
            tx.commit()?;
            Ok(running)
        })
        .await
    }

    /// Re-run a finished run's failed (or all) jobs as a fresh attempt.
    pub async fn retry_run(&self, run_id: String, only_failed: bool) -> anyhow::Result<u32> {
        self.call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            let filter = if only_failed {
                "status IN ('failed','cancelled','timed_out','skipped')"
            } else {
                "1 = 1"
            };
            let n = tx.execute(
                &format!(
                    "UPDATE jobs SET status = CASE WHEN needs = '[]' THEN 'queued' ELSE 'pending' END,
                       attempt = attempt + 1, agent = NULL, started_at = NULL, finished_at = NULL,
                       exit_code = NULL, error = NULL, queued_at = ?2
                     WHERE run_id = ?1 AND {filter}"
                ),
                params![run_id, now],
            )?;
            tx.execute(
                "UPDATE steps SET status = 'pending', started_at = NULL, finished_at = NULL, exit_code = NULL
                 WHERE job_id IN (SELECT id FROM jobs WHERE run_id = ?1 AND status IN ('queued','pending'))",
                [&run_id],
            )?;
            tx.execute(
                "UPDATE runs SET status = 'queued', finished_at = NULL, error = NULL WHERE id = ?1",
                [&run_id],
            )?;
            // Pending jobs whose needs already succeeded become queued immediately.
            advance_run(&tx, &run_id, now)?;
            tx.commit()?;
            Ok(u32::try_from(n).unwrap_or(u32::MAX))
        })
        .await
    }

    /// The runs of `group` that run `new` supersedes, as GitHub's concurrency groups have it: with
    /// `cancel-in-progress`, every older run still active; without it, the older ones waiting, as the
    /// oldest active goes on and `new` waits for it ([`Store::queued_jobs`]).
    pub async fn superseded_in_group(&self, group: String, new: String, cancel_in_progress: bool) -> anyhow::Result<Vec<String>> {
        self.call(move |c| {
            let mut st = c.prepare(
                "SELECT id FROM runs WHERE concurrency_group = ?1 AND id != ?2
                   AND status IN ('queued','running','pending') ORDER BY created_at, rowid",
            )?;
            let older = st.query_map(params![group, new], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(older.into_iter().skip(usize::from(!cancel_in_progress)).collect())
        })
        .await
    }

    /// The last day's runs and jobs (the overview's), every repository's or `among` these (by name:
    /// a person of some repositories, W10).
    pub async fn stats(&self, among: Option<Vec<String>>) -> anyhow::Result<serde_json::Value> {
        let among = among.map(|names| serde_json::to_string(&names).unwrap_or_default());
        self.call(move |c| {
            let since = crate::util::now_unix() - 86_400;
            // `?2`: the repositories, or NULL for all of them.
            const RUNS_IN: &str = "(?2 IS NULL OR repo_id IN (SELECT id FROM repos WHERE name IN (SELECT value FROM json_each(?2))))";
            const JOBS_IN: &str = "(?2 IS NULL OR run_id IN (SELECT r.id FROM runs r JOIN repos p ON p.id = r.repo_id WHERE p.name IN (SELECT value FROM json_each(?2))))";
            let count = |sql: &str| -> rusqlite::Result<i64> { c.query_row(&sql.replace("{RUNS_IN}", RUNS_IN), params![since, among], |r| r.get(0)) };
            let mut by_status = serde_json::Map::new();
            {
                let mut st = c.prepare(&format!("SELECT status, COUNT(*) FROM jobs WHERE (COALESCE(finished_at, queued_at, 0) >= ?1 OR status IN ('queued','running')) AND {JOBS_IN} GROUP BY status"))?;
                let rows = st.query_map(params![since, among], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                for row in rows {
                    let (s, n) = row?;
                    by_status.insert(s, n.into());
                }
            }
            let mut hourly = Vec::new();
            {
                let mut st = c.prepare(&format!(
                    "SELECT (finished_at / 3600) * 3600 AS h,
                            SUM(status = 'succeeded'), SUM(status IN ('failed','timed_out')),
                            AVG(finished_at - started_at)
                     FROM jobs WHERE finished_at >= ?1 AND {JOBS_IN} GROUP BY h ORDER BY h"
                ))?;
                let rows = st.query_map(params![since, among], |r| {
                    Ok(serde_json::json!({
                        "ts": r.get::<_, i64>(0)?,
                        "succeeded": r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        "failed": r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        "avg_seconds": r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                    }))
                })?;
                let mut found = std::collections::HashMap::new();
                for row in rows {
                    let v = row?;
                    found.insert(v.get("ts").and_then(serde_json::Value::as_i64).unwrap_or(0), v);
                }
                // A fixed 24-bucket series (zero-filled) so charts keep a real time axis.
                let first = (since / 3600 + 1) * 3600;
                for h in 0..24 {
                    let ts = first + h * 3600;
                    hourly.push(found.remove(&ts).unwrap_or_else(|| {
                        serde_json::json!({ "ts": ts, "succeeded": 0, "failed": 0, "avg_seconds": 0.0 })
                    }));
                }
            }
            Ok(serde_json::json!({
                "runs_24h": count("SELECT COUNT(*) FROM runs WHERE created_at >= ?1 AND {RUNS_IN}")?,
                "runs_failed_24h": count("SELECT COUNT(*) FROM runs WHERE created_at >= ?1 AND status IN ('failed','timed_out') AND {RUNS_IN}")?,
                "runs_succeeded_24h": count("SELECT COUNT(*) FROM runs WHERE created_at >= ?1 AND status = 'succeeded' AND {RUNS_IN}")?,
                "median_job_seconds": c.query_row(
                    &format!("SELECT finished_at - started_at AS d FROM jobs WHERE finished_at >= ?1 AND started_at IS NOT NULL AND {JOBS_IN}
                     ORDER BY d LIMIT 1 OFFSET (SELECT COUNT(*) / 2 FROM jobs WHERE finished_at >= ?1 AND started_at IS NOT NULL AND {JOBS_IN})"),
                    params![since, among], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0),
                "jobs_by_status": by_status,
                "hourly": hourly,
            }))
        })
        .await
    }

    /// How many jobs of these repositories (by name) wait and run: a person of some repositories'
    /// queue (W10), where the server's counts are everyone's.
    pub async fn queue_among(&self, names: Vec<String>) -> anyhow::Result<serde_json::Value> {
        let names = serde_json::to_string(&names)?;
        self.call(move |c| {
            let (queued, running): (i64, i64) = c.query_row(
                "SELECT COALESCE(SUM(j.status = 'queued'), 0), COALESCE(SUM(j.status = 'running'), 0) FROM jobs j JOIN runs r ON r.id = j.run_id JOIN repos p ON p.id = r.repo_id
                 WHERE j.status IN ('queued', 'running') AND p.name IN (SELECT value FROM json_each(?1))",
                [&names],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(serde_json::json!({ "queued": queued, "running": running }))
        })
        .await
    }

    /// Settle a run that has no jobs (e.g. its pipeline failed to parse).
    pub async fn finish_job_less_run(&self, id: String, status: Status) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute(
                "UPDATE runs SET status = ?2, finished_at = COALESCE(finished_at, ?3) WHERE id = ?1",
                params![id, status.as_str(), crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await
    }

    // ---------------------------------------------------------------- agents

    pub async fn upsert_agent(&self, a: AgentRow) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO agents (name, labels, slots, hostname, os, arch, version, ephemeral,
                   connected, last_seen, enrolled_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(name) DO UPDATE SET labels = excluded.labels, slots = excluded.slots,
                   hostname = excluded.hostname, os = excluded.os, arch = excluded.arch,
                   version = excluded.version, ephemeral = excluded.ephemeral,
                   connected = excluded.connected, last_seen = excluded.last_seen",
                params![
                    a.name,
                    json(&a.labels),
                    a.slots,
                    a.hostname,
                    a.os,
                    a.arch,
                    a.version,
                    a.ephemeral,
                    a.connected,
                    a.last_seen,
                    a.enrolled_at
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn agent_disconnected(&self, name: String) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute(
                "UPDATE agents SET connected = 0, last_seen = ?2 WHERE name = ?1",
                params![name, crate::util::now_unix()],
            )?;
            // Ephemeral (pod) agents are forgotten once gone.
            c.execute("DELETE FROM agents WHERE name = ?1 AND ephemeral = 1", [name])?;
            Ok(())
        })
        .await
    }

    pub async fn agents(&self) -> anyhow::Result<Vec<AgentRow>> {
        self.call(|c| {
            let mut st = c.prepare(
                "SELECT name, labels, slots, hostname, os, arch, version, ephemeral, connected,
                        last_seen, enrolled_at FROM agents ORDER BY connected DESC, name",
            )?;
            let rows = st.query_map([], |r| {
                Ok(AgentRow {
                    name: r.get(0)?,
                    labels: from_json(&r.get::<_, String>(1)?),
                    slots: r.get(2)?,
                    hostname: r.get(3)?,
                    os: r.get(4)?,
                    arch: r.get(5)?,
                    version: r.get(6)?,
                    ephemeral: r.get(7)?,
                    connected: r.get(8)?,
                    last_seen: r.get(9)?,
                    enrolled_at: r.get(10)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    pub async fn delete_agent(&self, name: String) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute("DELETE FROM agents WHERE name = ?1", [name])?;
            Ok(())
        })
        .await
    }

    // ---------------------------------------------------------------- key/value

    /// The value stored under `key`.
    pub async fn kv_get(&self, key: String) -> anyhow::Result<Option<String>> {
        self.call(move |c| {
            let v = c.query_row("SELECT v FROM kv WHERE k = ?1", [key], |r| r.get::<_, String>(0)).optional()?;
            Ok(v)
        })
        .await
    }

    /// Store `value` under `key` (replacing what was there).
    pub async fn kv_set(&self, key: String, value: String) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute("INSERT INTO kv (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v", params![key, value])?;
            Ok(())
        })
        .await
    }

    pub async fn kv_delete(&self, key: String) -> anyhow::Result<()> {
        self.call(move |c| {
            c.execute("DELETE FROM kv WHERE k = ?1", [key])?;
            Ok(())
        })
        .await
    }

    /// Every key starting with `prefix`, with its value, in key order.
    pub async fn kv_prefix(&self, prefix: String) -> anyhow::Result<Vec<(String, String)>> {
        self.call(move |c| {
            // `prefix` matched literally: substr instead of LIKE, whose `%` and `_` are wildcards.
            let mut st = c.prepare("SELECT k, v FROM kv WHERE substr(k, 1, length(?1)) = ?1 ORDER BY k")?;
            let rows = st.query_map([prefix], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    // ---------------------------------------------------------------- misc

    /// Records what `actor` did (docs/ci.md W19-R05). Within a request that changes
    /// something, it becomes part of that request's one row; elsewhere (a leg reading secrets, a
    /// background exchange, a GET's terminal session) it is a row of its own.
    pub async fn audit(&self, actor: String, action: String, detail: String) {
        if crate::audit::note(&actor, &action, &detail) {
            return;
        }
        let (principal, on_behalf_of) = match actor.split_once(" for ").or_else(|| actor.split_once(" of ")) {
            Some((p, b)) => (p.to_owned(), Some(b.to_owned())),
            None => (actor, None),
        };
        crate::audit::write(self, crate::audit::Entry { principal, on_behalf_of, action, result: "ok".into(), detail, ..Default::default() }).await;
    }

    /// Delete runs (and their logs, via the returned job ids) older than `days`.
    pub async fn prune(&self, days: u32) -> anyhow::Result<Vec<String>> {
        self.call(move |c| {
            let cutoff = crate::util::now_unix() - i64::from(days) * 86_400;
            let tx = crate::store::tx(c)?;
            let ids: Vec<String> = {
                let mut st = tx.prepare(
                    "SELECT j.id FROM jobs j JOIN runs r ON r.id = j.run_id
                     WHERE r.created_at < ?1 AND r.status NOT IN ('queued','running','pending')",
                )?;
                st.query_map([cutoff], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?
            };
            tx.execute(
                "DELETE FROM runs WHERE created_at < ?1 AND status NOT IN ('queued','running','pending')",
                [cutoff],
            )?;
            tx.commit()?;
            Ok(ids)
        })
        .await
    }
}

const JOB_SELECT: &str = "SELECT id, run_id, key, display, status, labels, needs, agent, executor,
    attempt, queued_at, started_at, finished_at, exit_code, error, peak_rss, cpu_seconds, spec, matrix FROM jobs";

fn row_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        repo_name: r.get(2)?,
        number: r.get(3)?,
        pipeline: r.get(4)?,
        event: r.get(5)?,
        branch: r.get(6)?,
        sha: r.get(7)?,
        message: r.get(8)?,
        author: r.get(9)?,
        status: Status::parse(&r.get::<_, String>(10)?),
        created_at: r.get(11)?,
        started_at: r.get(12)?,
        finished_at: r.get(13)?,
        error: r.get(14)?,
        concurrency_group: r.get(15)?,
    })
}

fn row_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        run_id: r.get(1)?,
        key: r.get(2)?,
        display: r.get(3)?,
        status: Status::parse(&r.get::<_, String>(4)?),
        labels: from_json(&r.get::<_, String>(5)?),
        needs: from_json(&r.get::<_, String>(6)?),
        agent: r.get(7)?,
        executor: r.get(8)?,
        attempt: r.get(9)?,
        queued_at: r.get(10)?,
        started_at: r.get(11)?,
        finished_at: r.get(12)?,
        exit_code: r.get(13)?,
        error: r.get(14)?,
        peak_rss: r.get::<_, Option<i64>>(15)?.and_then(|v| u64::try_from(v).ok()),
        cpu_seconds: r.get(16)?,
        spec: serde_json::from_str(&r.get::<_, String>(17)?).ok(),
        steps: Vec::new(),
        matrix: r
            .get::<_, Option<String>>(18)?
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .filter(|v| v.as_object().is_some_and(|o| !o.is_empty())),
    })
}

fn steps_of(c: &Connection, job_id: &str) -> rusqlite::Result<Vec<Step>> {
    let mut st = c.prepare_cached(
        "SELECT idx, name, status, started_at, finished_at, exit_code, started_seq FROM steps
         WHERE job_id = ?1 ORDER BY idx",
    )?;
    st.query_map([job_id], |r| {
        Ok(Step {
            idx: r.get(0)?,
            name: r.get(1)?,
            status: Status::parse(&r.get::<_, String>(2)?),
            started_at: r.get(3)?,
            finished_at: r.get(4)?,
            exit_code: r.get(5)?,
            seq: r.get(6)?,
        })
    })?
    .collect()
}

/// Promote pending jobs whose dependencies all succeeded, skip those with a failed dependency, and
/// settle the run when every job is terminal.
const DEP_FAILED: &str = "dependency failed";

fn advance_run(tx: &Connection, run_id: &str, now: i64) -> anyhow::Result<Option<Status>> {
    loop {
        let jobs: Vec<(String, String, Vec<String>)> = {
            let mut st = tx.prepare("SELECT id, key, status, needs, COALESCE(error, '') FROM jobs WHERE run_id = ?1")?;
            let rows = st.query_map([run_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?;
            let mut v = Vec::new();
            let mut status_by_key = std::collections::HashMap::new();
            for row in rows {
                let (id, key, status, needs, error) = row?;
                let mut st = Status::parse(&status);
                // A job skipped because its own dependency failed propagates the failure.
                if st == Status::Skipped && error == DEP_FAILED {
                    st = Status::Failed;
                }
                status_by_key.insert(key.clone(), st);
                v.push((id, key, status, needs));
            }
            let mut changes = Vec::new();
            for (id, _key, status, needs) in v {
                if Status::parse(&status) != Status::Pending {
                    continue;
                }
                let needs: Vec<String> = from_json(&needs);
                let states: Vec<Status> = needs
                    .iter()
                    .map(|n| status_by_key.get(n).copied().unwrap_or(Status::Succeeded))
                    .collect();
                if states.iter().any(|s| s.is_terminal() && !s.is_success()) {
                    changes.push((id, "skipped".to_owned(), needs));
                } else if states.iter().all(|s| s.is_success()) {
                    changes.push((id, "queued".to_owned(), needs));
                }
            }
            changes
        };
        if jobs.is_empty() {
            break;
        }
        for (id, status, _) in &jobs {
            if status == "queued" {
                tx.execute(
                    "UPDATE jobs SET status = 'queued', queued_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
            } else {
                tx.execute(
                    "UPDATE jobs SET status = 'skipped', finished_at = ?2, error = ?3 WHERE id = ?1",
                    params![id, now, DEP_FAILED],
                )?;
                tx.execute("UPDATE steps SET status = 'skipped' WHERE job_id = ?1", [id])?;
            }
        }
    }
    let statuses: Vec<Status> = {
        let mut st = tx.prepare("SELECT status FROM jobs WHERE run_id = ?1")?;
        st.query_map([run_id], |r| r.get::<_, String>(0))?
            .map(|s| s.map(|s| Status::parse(&s)))
            .collect::<Result<Vec<_>, _>>()?
    };
    if statuses.iter().all(|s| s.is_terminal()) {
        let status = if statuses.contains(&Status::Cancelled) {
            Status::Cancelled
        } else if statuses.contains(&Status::TimedOut) {
            Status::TimedOut
        } else if statuses.iter().any(|s| !s.is_success()) {
            Status::Failed
        } else {
            Status::Succeeded
        };
        tx.execute(
            "UPDATE runs SET status = ?2, finished_at = ?3 WHERE id = ?1",
            params![run_id, status.as_str(), now],
        )?;
        return Ok(Some(status));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline;

    #[test]
    fn a_repository_that_moves_adopts_its_new_remote() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let s = Store::in_memory()?;
            let at = |url: &str| Repo {
                id: crate::util::new_id(),
                name: "hyper-light/kiln".into(),
                clone_url: url.into(),
                branches: vec![],
                poll_seconds: 60,
                enabled: true,
                created_at: 0,
                last_polled_at: None,
                last_error: None,
                mode: "upstream".into(),
                default_branch: None,
                mirror: None,
            };
            let repo = s.upsert_repo(at("http://kiln-git/kiln.git")).await?;
            s.set_ref(repo.id.clone(), "refs/heads/main".into(), "a1".into()).await?;
            // Re-read from the same remote (every discovery does): what it saw stays.
            let same = s.upsert_repo(at("http://kiln-git/kiln.git")).await?;
            anyhow::ensure!(same.id == repo.id && s.refs(repo.id.clone()).await?.len() == 1, "refs lost on a plain re-read");
            // Moved to GitHub: same repository, nothing seen yet there (its next poll adopts it).
            let moved = s.upsert_repo(at("https://github.com/hyper-light/kiln.git")).await?;
            anyhow::ensure!(moved.id == repo.id, "a moved repository keeps its id (and its runs)");
            anyhow::ensure!(s.refs(repo.id.clone()).await?.is_empty(), "refs from the old remote survived the move");
            anyhow::Ok(())
        })
    }

    /// The runners a server counts as connected just before it started, so a deploy can check
    /// they came back: still marked connected, or gone in the last minutes; not a job's pod.
    #[test]
    fn runners_connected_before_a_start() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let s = Store::in_memory()?;
            let now = crate::util::now_unix();
            let runner = |name: &str, connected: bool, last_seen: i64, ephemeral: bool| AgentRow {
                name: name.into(),
                labels: vec![],
                slots: 1,
                hostname: name.into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                version: "1".into(),
                ephemeral,
                connected,
                last_seen,
                enrolled_at: 0,
            };
            s.upsert_agent(runner("killed-with-its-server", true, now - 86_400, false)).await?;
            s.upsert_agent(runner("gone-at-its-shutdown", false, now - 30, false)).await?;
            s.upsert_agent(runner("asleep-for-hours", false, now - 7_200, false)).await?;
            s.upsert_agent(runner("a-jobs-pod", false, now - 30, true)).await?;
            anyhow::ensure!(s.runners_before(900).await? == 2);
            s.recover().await?;
            anyhow::ensure!(s.runners_before(900).await? == 1, "recovering marks them all gone; read it first");
            anyhow::Ok(())
        })
    }

    #[test]
    fn key_values_are_kept_and_listed_by_prefix() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let s = Store::in_memory()?;
            assert_eq!(s.kv_get("front_door/box".into()).await?, None);
            s.kv_set("front_door/box".into(), "{}".into()).await?;
            s.kv_set("front_door/box".into(), r#"{"domain":"kiln.home"}"#.into()).await?;
            s.kv_set("front_door/a_b".into(), "1".into()).await?;
            s.kv_set("front_doorX".into(), "no".into()).await?;
            assert_eq!(s.kv_get("front_door/box".into()).await?.as_deref(), Some(r#"{"domain":"kiln.home"}"#));
            let all = s.kv_prefix("front_door/".into()).await?;
            assert_eq!(all.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["front_door/a_b", "front_door/box"]);
            // `_` and `%` in a prefix are plain characters.
            assert_eq!(s.kv_prefix("front_door/a_".into()).await?.len(), 1);
            assert!(s.kv_prefix("front_door/a%".into()).await?.is_empty());
            s.kv_delete("front_door/box".into()).await?;
            assert_eq!(s.kv_get("front_door/box".into()).await?, None);
            anyhow::Ok(())
        })
    }

    #[test]
    fn run_lifecycle() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
        let s = Store::in_memory()?;
        let repo = s
            .upsert_repo(Repo {
                id: "r1".into(),
                name: "hyper-light/x".into(),
                clone_url: "https://example/x".into(),
                branches: vec![],
                poll_seconds: 60,
                enabled: true,
                created_at: 0,
                last_polled_at: None,
                last_error: None,
                mode: "upstream".into(),
                default_branch: None,
                mirror: None,
            })
            .await?;
        let p = pipeline::parse(
            "name: ci\njobs:\n  a: {steps: [{run: x}]}\n  b: {needs: [a], steps: [{run: y}]}\n  c: {needs: [b], steps: [{run: z}]}\n",
        )?;
        let planned = pipeline::plan(&p, &pipeline::Context::default())?;
        let run = s
            .create_run(
                Run {
                    id: "run1".into(),
                    repo_id: repo.id.clone(),
                    repo_name: repo.name.clone(),
                    number: 0,
                    pipeline: "ci".into(),
                    event: "push".into(),
                    branch: "main".into(),
                    sha: "abc".into(),
                    message: String::new(),
                    author: String::new(),
                    status: Status::Queued,
                    created_at: 0,
                    started_at: None,
                    finished_at: None,
                    error: None,
                    concurrency_group: None,
                },
                planned,
            )
            .await?;
        assert_eq!(run.number, 1);
        let q = s.queued_jobs(10).await?;
        assert_eq!(q.len(), 1);
        let a = q.first().map(|j| j.id.clone()).unwrap_or_default();
        assert!(s.claim_job(a.clone(), "ag".into(), "baremetal".into()).await?);
        assert!(!s.claim_job(a.clone(), "ag2".into(), "baremetal".into()).await?);
        // A declined job goes back to the queue without spending an attempt.
        s.unclaim_job(a.clone(), "declined".into()).await?;
        let back = s.job(a.clone()).await?.ok_or_else(|| anyhow::anyhow!("job vanished"))?;
        assert_eq!((back.status, back.attempt), (Status::Queued, 0));
        // Running jobs survive a server restart untouched, awaiting reclamation by their agent.
        assert!(s.claim_job(a.clone(), "ag".into(), "baremetal".into()).await?);
        assert_eq!(s.recover().await?, 1);
        assert_eq!(s.job(a.clone()).await?.map(|j| j.status), Some(Status::Running));
        let (_, settled) = s.finish_job(a, Status::Failed, Some(1), None, None, None).await?;
        assert_eq!(settled, Some(Status::Failed));
        let jobs = s.jobs("run1".into(), false).await?;
        assert!(jobs.iter().filter(|j| j.key != "a").all(|j| j.status == Status::Skipped));
        // retry only failed: a goes back to queued, b/c pending again.
        s.retry_run("run1".into(), true).await?;
        assert_eq!(s.queued_jobs(10).await?.len(), 1);
        Ok(())
    })
    }

    /// A concurrency group runs one run at a time: a newer run waits for the one under way, and a
    /// newer one still drops the one waiting (the image pipeline built seven at once without it).
    #[test]
    fn a_group_runs_one_at_a_time() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let s = Store::in_memory()?;
            let repo = s
                .upsert_repo(Repo {
                    id: "r1".into(),
                    name: "hyper-light/x".into(),
                    clone_url: "https://example/x".into(),
                    branches: vec![],
                    poll_seconds: 60,
                    enabled: true,
                    created_at: 0,
                    last_polled_at: None,
                    last_error: None,
                    mode: "upstream".into(),
                    default_branch: None,
                    mirror: None,
                })
                .await?;
            let p = pipeline::parse("name: image\njobs:\n  image: {steps: [{run: x}]}\n")?;
            let run = |id: &str, at: i64, group: Option<&str>| Run {
                id: id.into(),
                repo_id: repo.id.clone(),
                repo_name: repo.name.clone(),
                number: 0,
                pipeline: "image".into(),
                event: "push".into(),
                branch: "main".into(),
                sha: id.into(),
                message: String::new(),
                author: String::new(),
                status: Status::Queued,
                created_at: at,
                started_at: None,
                finished_at: None,
                error: None,
                concurrency_group: group.map(str::to_owned),
            };
            for (id, at, group) in [("a", 1, Some("x:image")), ("b", 2, Some("x:image")), ("c", 2, Some("x:image")), ("free", 3, None)] {
                s.create_run(run(id, at, group), pipeline::plan(&p, &pipeline::Context::default())?).await?;
            }
            let waiting = |s: Store| async move { anyhow::Ok(s.queued_jobs(10).await?.into_iter().map(|j| j.run_id).collect::<Vec<_>>()) };
            // The oldest of the group goes, and a run of no group; b and c wait.
            assert_eq!(waiting(s.clone()).await?, ["a", "free"]);
            // c, the newest, drops b (waiting) and leaves a (under way), or cancels both.
            assert_eq!(s.superseded_in_group("x:image".into(), "c".into(), false).await?, ["b"]);
            assert_eq!(s.superseded_in_group("x:image".into(), "c".into(), true).await?, ["a", "b"]);
            s.cancel_run("b".into(), "superseded by a newer run".into()).await?;
            let a = s.jobs("a".into(), false).await?.first().map(|j| j.id.clone()).unwrap_or_default();
            assert!(s.claim_job(a.clone(), "ag".into(), "baremetal".into()).await?);
            assert_eq!(waiting(s.clone()).await?, ["free"], "c waits while a runs");
            s.finish_job(a, Status::Succeeded, Some(0), None, None, None).await?;
            assert_eq!(waiting(s.clone()).await?, ["c", "free"], "c goes once a is done");
            Ok(())
        })
    }
}
