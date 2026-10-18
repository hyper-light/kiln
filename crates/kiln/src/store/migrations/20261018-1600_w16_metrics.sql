-- Benchmark metrics (docs/ci.md W16-R07, R08): each point a step wrote, with the runner's
-- fingerprint, and how it compared with its baseline when it came.
CREATE TABLE IF NOT EXISTS metrics (
    id INTEGER PRIMARY KEY,
    repo_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    leg TEXT NOT NULL,
    leg_key TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    branch TEXT NOT NULL,
    sha TEXT NOT NULL,
    step_idx INTEGER,
    name TEXT NOT NULL,
    value REAL NOT NULL,
    unit TEXT NOT NULL,
    better TEXT NOT NULL,
    tags TEXT,
    samples TEXT,
    fingerprint TEXT NOT NULL,
    -- `{ verdict, change, low, high, runs }` against the baseline, or null without one.
    comparison TEXT,
    ts INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS metrics_name ON metrics (repo_id, name, branch, fingerprint, ts);
CREATE INDEX IF NOT EXISTS metrics_leg ON metrics (leg);
CREATE INDEX IF NOT EXISTS metrics_run ON metrics (run_id);
