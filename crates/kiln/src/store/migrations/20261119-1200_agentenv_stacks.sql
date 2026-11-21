-- Environments' scopes, stacks, and applied on their own (`crate::agentenvs`, docs/agents.md "Scopes, stacks, and
-- applied on its own"; decided by the user, 2026-11-19).
-- agentenvs.scope: where it's offered and applied on its own, chips in order, JSON (each `{key, value}`); none,
-- everywhere. agentenvs.auto: 1 where it joins the stacks of its owner's sessions its scope matches on its own.
-- agentenv_stacks: each session's environments in order, `position` 0 its top, each once; `auto` 1 where it joined on
-- its own (and leaves on its own), 0 where it was given by name. A session with none has no row. Each row of
-- agentenv_sessions, the one environment a session ran with until now, is its stack's top, given by name.
-- agentenv_sessions stays, read no more: a kiln from before reads it as it starts, so dropping it would need an epoch
-- (docs/resilience.md G2) and a rollback past this build would be refused. New columns with defaults and a new table:
-- no epoch.
ALTER TABLE agentenvs ADD COLUMN scope TEXT NOT NULL DEFAULT '[]';
ALTER TABLE agentenvs ADD COLUMN auto INTEGER NOT NULL DEFAULT 0;

CREATE TABLE agentenv_stacks (
    session TEXT NOT NULL,
    position INTEGER NOT NULL,
    agentenv_id TEXT NOT NULL,
    auto INTEGER NOT NULL DEFAULT 0,
    applied_at INTEGER NOT NULL,
    applied_by TEXT NOT NULL,
    PRIMARY KEY (session, position),
    UNIQUE (session, agentenv_id)
);

CREATE INDEX agentenv_stacks_agentenv ON agentenv_stacks (agentenv_id);

INSERT INTO agentenv_stacks (session, position, agentenv_id, auto, applied_at, applied_by)
SELECT session, 0, agentenv_id, 0, applied_at, applied_by FROM agentenv_sessions;
