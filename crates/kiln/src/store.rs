//! Durable state in SQLite (WAL). Every call runs on the blocking pool so the async runtime is
//! never stalled by disk I/O. Logs are *not* stored here (see `logs.rs`).

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::pipeline::PlannedJob;

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub idx: u32,
    pub name: String,
    pub status: Status,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
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
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("create {}", d.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("apply schema")?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Run `f` with the connection on the blocking pool.
    pub async fn call<R, F>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut g = conn.lock();
            f(&mut g)
        })
        .await
        .map_err(|e| anyhow::anyhow!("store task failed: {e}"))?
    }

    /// After an unclean stop: anything that was running is requeued (bare-metal jobs whose agent
    /// is gone) and every agent is marked disconnected until it reconnects.
    pub async fn recover(&self) -> anyhow::Result<usize> {
        self.call(|c| {
            let tx = c.transaction()?;
            tx.execute("UPDATE agents SET connected = 0", [])?;
            let n = tx.execute(
                "UPDATE jobs SET status = 'queued', agent = NULL, attempt = attempt + 1,
                   error = 'requeued after server restart' WHERE status = 'running'",
                [],
            )?;
            tx.execute(
                "UPDATE steps SET status = 'pending', started_at = NULL WHERE status = 'running'",
                [],
            )?;
            tx.commit()?;
            Ok(n)
        })
        .await
    }

    // ---------------------------------------------------------------- repos

    pub async fn upsert_repo(&self, r: Repo) -> anyhow::Result<Repo> {
        self.call(move |c| {
            c.execute(
                "INSERT INTO repos (id, name, clone_url, branches, poll_seconds, enabled, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(name) DO UPDATE SET clone_url = excluded.clone_url,
                   branches = excluded.branches, poll_seconds = excluded.poll_seconds,
                   enabled = excluded.enabled",
                params![
                    r.id,
                    r.name,
                    r.clone_url,
                    json(&r.branches),
                    r.poll_seconds,
                    r.enabled,
                    r.created_at
                ],
            )?;
            let id: String =
                c.query_row("SELECT id FROM repos WHERE name = ?1", [&r.name], |row| row.get(0))?;
            Ok(Repo { id, ..r })
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
                        last_polled_at, last_error FROM repos ORDER BY name",
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
            let tx = c.transaction()?;
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
        self.call(move |c| {
            let mut st = c.prepare(&format!("{JOB_SELECT} WHERE run_id = ?1 ORDER BY rowid"))?;
            let mut jobs: Vec<Job> =
                st.query_map([&run_id], row_job)?.collect::<Result<Vec<_>, _>>()?;
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

    /// Jobs waiting for an agent, oldest first.
    pub async fn queued_jobs(&self, limit: u32) -> anyhow::Result<Vec<Job>> {
        self.call(move |c| {
            let mut st = c.prepare(&format!(
                "{JOB_SELECT} WHERE status = 'queued' ORDER BY queued_at, rowid LIMIT ?1"
            ))?;
            Ok(st.query_map([limit], row_job)?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    pub async fn running_jobs(&self) -> anyhow::Result<Vec<Job>> {
        self.call(|c| {
            let mut st = c.prepare(&format!("{JOB_SELECT} WHERE status = 'running'"))?;
            Ok(st.query_map([], row_job)?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Atomically claim a queued job for an agent. Returns false if someone else got it.
    pub async fn claim_job(&self, id: String, agent: String, executor: String) -> anyhow::Result<bool> {
        self.call(move |c| {
            let tx = c.transaction()?;
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
            let tx = c.transaction()?;
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
            let tx = c.transaction()?;
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
            let tx = c.transaction()?;
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
            let tx = c.transaction()?;
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

    /// Runs in the same concurrency group that are still active (for cancel-in-progress).
    pub async fn active_runs_in_group(&self, group: String, except: String) -> anyhow::Result<Vec<String>> {
        self.call(move |c| {
            let mut st = c.prepare(
                "SELECT id FROM runs WHERE concurrency_group = ?1 AND id != ?2
                   AND status IN ('queued','running','pending')",
            )?;
            Ok(st.query_map(params![group, except], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    pub async fn stats(&self) -> anyhow::Result<serde_json::Value> {
        self.call(|c| {
            let since = crate::util::now_unix() - 86_400;
            let count = |sql: &str| -> rusqlite::Result<i64> { c.query_row(sql, [since], |r| r.get(0)) };
            let mut by_status = serde_json::Map::new();
            {
                let mut st =
                    c.prepare("SELECT status, COUNT(*) FROM jobs WHERE COALESCE(finished_at, queued_at, 0) >= ?1 OR status IN ('queued','running') GROUP BY status")?;
                let rows = st.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                for row in rows {
                    let (s, n) = row?;
                    by_status.insert(s, n.into());
                }
            }
            let mut hourly = Vec::new();
            {
                let mut st = c.prepare(
                    "SELECT (finished_at / 3600) * 3600 AS h,
                            SUM(status = 'succeeded'), SUM(status IN ('failed','timed_out')),
                            AVG(finished_at - started_at)
                     FROM jobs WHERE finished_at >= ?1 GROUP BY h ORDER BY h",
                )?;
                let rows = st.query_map([since], |r| {
                    Ok(serde_json::json!({
                        "ts": r.get::<_, i64>(0)?,
                        "succeeded": r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        "failed": r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        "avg_seconds": r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                    }))
                })?;
                for row in rows {
                    hourly.push(row?);
                }
            }
            Ok(serde_json::json!({
                "runs_24h": count("SELECT COUNT(*) FROM runs WHERE created_at >= ?1")?,
                "runs_failed_24h": count("SELECT COUNT(*) FROM runs WHERE created_at >= ?1 AND status IN ('failed','timed_out')")?,
                "runs_succeeded_24h": count("SELECT COUNT(*) FROM runs WHERE created_at >= ?1 AND status = 'succeeded'")?,
                "median_job_seconds": c.query_row(
                    "SELECT finished_at - started_at AS d FROM jobs WHERE finished_at >= ?1 AND started_at IS NOT NULL
                     ORDER BY d LIMIT 1 OFFSET (SELECT COUNT(*) / 2 FROM jobs WHERE finished_at >= ?1 AND started_at IS NOT NULL)",
                    [since], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0),
                "jobs_by_status": by_status,
                "hourly": hourly,
            }))
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

    // ---------------------------------------------------------------- misc

    pub async fn audit(&self, actor: String, action: String, detail: String) {
        let r = self
            .call(move |c| {
                c.execute(
                    "INSERT INTO audit (ts, actor, action, detail) VALUES (?1, ?2, ?3, ?4)",
                    params![crate::util::now_unix(), actor, action, detail],
                )?;
                Ok(())
            })
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "audit write failed");
        }
    }

    pub async fn audit_log(&self, limit: u32) -> anyhow::Result<Vec<serde_json::Value>> {
        self.call(move |c| {
            let mut st =
                c.prepare("SELECT ts, actor, action, detail FROM audit ORDER BY id DESC LIMIT ?1")?;
            let rows = st.query_map([limit.min(1000)], |r| {
                Ok(serde_json::json!({
                    "ts": r.get::<_, i64>(0)?, "actor": r.get::<_, String>(1)?,
                    "action": r.get::<_, String>(2)?, "detail": r.get::<_, String>(3)?,
                }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Delete runs (and their logs, via the returned job ids) older than `days`.
    pub async fn prune(&self, days: u32) -> anyhow::Result<Vec<String>> {
        self.call(move |c| {
            let cutoff = crate::util::now_unix() - i64::from(days) * 86_400;
            let tx = c.transaction()?;
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
            tx.execute("DELETE FROM audit WHERE ts < ?1", [cutoff])?;
            tx.commit()?;
            Ok(ids)
        })
        .await
    }
}

const JOB_SELECT: &str = "SELECT id, run_id, key, display, status, labels, needs, agent, executor,
    attempt, queued_at, started_at, finished_at, exit_code, error, peak_rss, cpu_seconds, spec FROM jobs";

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
    })
}

fn steps_of(c: &Connection, job_id: &str) -> rusqlite::Result<Vec<Step>> {
    let mut st = c.prepare_cached(
        "SELECT idx, name, status, started_at, finished_at, exit_code FROM steps
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
        })
    })?
    .collect()
}

/// Promote pending jobs whose dependencies all succeeded, skip those with a failed dependency, and
/// settle the run when every job is terminal.
const DEP_FAILED: &str = "dependency failed";

fn advance_run(tx: &rusqlite::Transaction<'_>, run_id: &str, now: i64) -> anyhow::Result<Option<Status>> {
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

    #[tokio::test]
    async fn run_lifecycle() -> anyhow::Result<()> {
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
        let (_, settled) = s.finish_job(a, Status::Failed, Some(1), None, None, None).await?;
        assert_eq!(settled, Some(Status::Failed));
        let jobs = s.jobs("run1".into(), false).await?;
        assert!(jobs.iter().filter(|j| j.key != "a").all(|j| j.status == Status::Skipped));
        // retry only failed: a goes back to queued, b/c pending again.
        s.retry_run("run1".into(), true).await?;
        assert_eq!(s.queued_jobs(10).await?.len(), 1);
        Ok(())
    }
}
