-- A plan run as one agent per step (docs/plans.md, "One agent per step"; docs/agentic.md P):
-- the run (where and how its steps run, and how it stands) and each step's agent.
ALTER TABLE plan_runs ADD COLUMN state TEXT;
ALTER TABLE plan_runs ADD COLUMN config TEXT;
ALTER TABLE plan_runs ADD COLUMN note TEXT;
ALTER TABLE plan_runs ADD COLUMN reason TEXT;
ALTER TABLE plan_runs ADD COLUMN ended_at INTEGER;
CREATE TABLE IF NOT EXISTS plan_run_steps (
    run_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    title TEXT NOT NULL,
    goal TEXT NOT NULL,
    -- waiting | started | taken | met | unmet | stopped
    status TEXT NOT NULL DEFAULT 'waiting',
    agent TEXT,
    started_at INTEGER,
    ended_at INTEGER,
    reason TEXT,
    tries INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, idx)
);
CREATE INDEX IF NOT EXISTS plan_runs_by_state ON plan_runs (state);
