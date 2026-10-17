-- Plans (docs/plans.md): what an agent will do, kept apart from any one session, changed (each
-- change a version), shared by a link and run at will on any agent.
CREATE TABLE IF NOT EXISTS plans (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT NOT NULL DEFAULT '[]',
    source TEXT,
    share TEXT UNIQUE,
    version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS plan_versions (
    plan_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    at INTEGER NOT NULL,
    by TEXT NOT NULL,
    PRIMARY KEY (plan_id, version)
);
CREATE TABLE IF NOT EXISTS plan_runs (
    id TEXT PRIMARY KEY,
    plan_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    session TEXT NOT NULL,
    mode TEXT NOT NULL,
    at INTEGER NOT NULL,
    by TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS plan_runs_by_plan ON plan_runs (plan_id, at);
