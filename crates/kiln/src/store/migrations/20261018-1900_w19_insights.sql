-- Insights (docs/ci.md W19-R01, R02; W08-R13).

-- A leg's queue time by wait reason (seconds, JSON), and when its current wait began.
ALTER TABLE jobs ADD COLUMN waits TEXT;
ALTER TABLE jobs ADD COLUMN wait_since INTEGER;

-- What finished when, for rolling up by the hour.
CREATE INDEX IF NOT EXISTS jobs_finished_v2 ON jobs (finished_at) WHERE job_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS runs_finished_v2 ON runs (finished_at) WHERE workflow_id IS NOT NULL;

-- Hourly and daily rollups: one row per bucket, repository, workflow, job, step, executor and
-- runner (`job` empty: the run as a whole; `step` empty: the leg). `data` is kiln_ci's Rollup.
CREATE TABLE IF NOT EXISTS ci_rollups (
    grain TEXT NOT NULL,             -- hour | day
    bucket INTEGER NOT NULL,         -- its start (Unix seconds, UTC)
    repo_id TEXT NOT NULL,
    workflow TEXT NOT NULL,
    job TEXT NOT NULL,
    step TEXT NOT NULL,
    executor TEXT NOT NULL,
    runner TEXT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (grain, bucket, repo_id, workflow, job, step, executor, runner)
);
CREATE INDEX IF NOT EXISTS ci_rollups_repo ON ci_rollups (grain, repo_id, bucket);
CREATE INDEX IF NOT EXISTS ci_rollups_runner ON ci_rollups (grain, runner, bucket);
-- The buckets rolled up.
CREATE TABLE IF NOT EXISTS ci_rollup_done (
    grain TEXT NOT NULL,
    bucket INTEGER NOT NULL,
    PRIMARY KEY (grain, bucket)
);

-- Each runner's time per hour, sampled every minute: connected, its slots × that, and slots busy.
CREATE TABLE IF NOT EXISTS ci_runner_time (
    bucket INTEGER NOT NULL,
    runner TEXT NOT NULL,
    connected REAL NOT NULL DEFAULT 0,
    slot_seconds REAL NOT NULL DEFAULT 0,
    busy_seconds REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket, runner)
);

-- Cache lookups and saves (W08-R13): hit, miss or save, by key, with the bytes.
CREATE TABLE IF NOT EXISTS ci_cache_events (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    repo_id TEXT NOT NULL,
    run_id TEXT,
    leg_id TEXT,
    key TEXT NOT NULL,
    kind TEXT NOT NULL,
    bytes INTEGER
);
CREATE INDEX IF NOT EXISTS ci_cache_events_repo ON ci_cache_events (repo_id, ts);
