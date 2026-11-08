-- Mission Control attention, plan 01a199e1b177752e84dad5dc990e6a15: private placement,
-- durable meaningful uses and a restorable archive independent of notice retention.
-- A turn's use (kind `turn` or `subagent_turn`) says which end of the turn it is: 'start' or 'end'.
CREATE TABLE IF NOT EXISTS attention_uses (
    principal TEXT NOT NULL,
    agent TEXT NOT NULL,
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    at INTEGER NOT NULL,
    edge TEXT CHECK (edge IN ('start', 'end')),
    PRIMARY KEY (principal, agent, kind, key)
);
CREATE INDEX IF NOT EXISTS attention_uses_person_time ON attention_uses (principal, at);
CREATE INDEX IF NOT EXISTS attention_uses_agent_time ON attention_uses (agent, at);
CREATE TABLE IF NOT EXISTS attention_placement (
    principal TEXT NOT NULL,
    agent TEXT NOT NULL,
    value TEXT NOT NULL,
    previous TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (principal, agent)
);
CREATE TABLE IF NOT EXISTS attention_settings (
    principal TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- Where each choice came from: a person's command, a direct pick (a notification, a link, search) or the list.
CREATE TABLE IF NOT EXISTS attention_choices (
    principal TEXT NOT NULL,
    key TEXT NOT NULL,
    at INTEGER NOT NULL,
    chosen TEXT NOT NULL,
    candidates TEXT NOT NULL,
    propensity REAL,
    source TEXT NOT NULL CHECK (source IN ('command', 'direct', 'list')),
    PRIMARY KEY (principal, key)
);
CREATE TABLE IF NOT EXISTS attention_archive (
    agent TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    at INTEGER NOT NULL,
    value TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'archiving'
);
CREATE TABLE IF NOT EXISTS attention_exposures (
    principal TEXT NOT NULL,
    key TEXT NOT NULL,
    at INTEGER NOT NULL,
    candidates TEXT NOT NULL,
    PRIMARY KEY (principal, key)
);
-- History from before the attention tables: a person's commands in the audit table up to this fixed fence count as
-- their uses, once; the hooks record every one after it, so none counts twice.
CREATE TABLE IF NOT EXISTS attention_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT OR IGNORE INTO attention_meta (key, value)
SELECT 'audit_fence', COALESCE(MAX(id), 0) FROM audit2;
CREATE TABLE IF NOT EXISTS attention_archive_groups (
    root TEXT PRIMARY KEY,
    members TEXT NOT NULL,
    status TEXT NOT NULL
);

-- Request times come from provider approvals, held goals and terminal errors, not a poll clock.
CREATE TABLE IF NOT EXISTS attention_requests (
    agent TEXT NOT NULL, key TEXT NOT NULL, row_agent TEXT NOT NULL, kind TEXT NOT NULL,
    provider_id TEXT,
    asked_at INTEGER, observed_at INTEGER NOT NULL, closed_at INTEGER,
    ambiguous INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent, key)
);
CREATE TABLE IF NOT EXISTS attention_request_heads (
    agent TEXT PRIMARY KEY, revision TEXT NOT NULL, observed_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS attention_response_attempts (
    principal TEXT NOT NULL, agent TEXT NOT NULL, event_key TEXT NOT NULL,
    selector TEXT NOT NULL, at INTEGER NOT NULL, matched INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (principal, agent, event_key, selector)
);
CREATE TABLE IF NOT EXISTS attention_responses (
    principal TEXT NOT NULL, agent TEXT NOT NULL, request_key TEXT NOT NULL,
    kind TEXT NOT NULL, duration INTEGER, at INTEGER NOT NULL,
    PRIMARY KEY (principal, agent, request_key)
);
