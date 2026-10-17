-- Goal stacks (docs/goals.md): an agent's goals, one after another, each pursued until it's met
-- before the next starts.
CREATE TABLE IF NOT EXISTS goal_stacks (
    agent TEXT PRIMARY KEY,
    state TEXT NOT NULL DEFAULT 'paused',
    reason TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_items (
    id TEXT PRIMARY KEY,
    agent TEXT NOT NULL,
    pos INTEGER NOT NULL,
    text TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    reason TEXT,
    how TEXT,
    prior_at INTEGER NOT NULL DEFAULT 0,
    saw_running INTEGER NOT NULL DEFAULT 0,
    started_at INTEGER,
    ended_at INTEGER
);
CREATE INDEX IF NOT EXISTS goal_items_by_agent ON goal_items (agent, pos);
