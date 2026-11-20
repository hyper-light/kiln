-- Environments for agent sessions (`crate::agentenvs`, docs/agents.md "Environments"; decided by the
-- user, 2026-11-18): each its owner's (`owner`, the server's owner, or `user:<id>`), named once among
-- that owner's, its variables kept as written (`vars`: JSON, each `{name, from, of?, value?, secret?}`,
-- a secret by its name, never its value), its own tags (`tags`: JSON, in order), its version up
-- with each change. New tables: an older kiln
-- never names them.
CREATE TABLE agentenvs (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    base TEXT NOT NULL,
    vars TEXT NOT NULL,
    tags TEXT NOT NULL DEFAULT '[]',
    version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (owner, name)
);

-- Which environment each agent session runs with, by the session's id: opened again whenever its
-- harness starts (started, restarted, resumed, moved), and an environment one runs with can't be
-- deleted. A session with none has no row.
CREATE TABLE agentenv_sessions (
    session TEXT PRIMARY KEY,
    agentenv_id TEXT NOT NULL,
    applied_at INTEGER NOT NULL,
    applied_by TEXT NOT NULL
);

CREATE INDEX agentenv_sessions_agentenv ON agentenv_sessions (agentenv_id);
