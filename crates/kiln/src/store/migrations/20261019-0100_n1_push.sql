-- Notifications to people's devices (docs/notifications.md, docs/agentic.md N1).
-- Each device a person turned notifications on for: the browser's push endpoint and keys, its
-- name, what it hears, and how its last delivery went.
CREATE TABLE IF NOT EXISTS push_subscriptions (
    id TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    endpoint TEXT NOT NULL UNIQUE,
    p256dh TEXT NOT NULL,
    auth TEXT NOT NULL,
    device TEXT NOT NULL DEFAULT '',
    hears TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_ok_at INTEGER,
    last_error TEXT,
    last_error_at INTEGER
);
CREATE INDEX IF NOT EXISTS push_subscriptions_principal ON push_subscriptions (principal);
-- Agents a person muted.
CREATE TABLE IF NOT EXISTS push_mutes (
    principal TEXT NOT NULL,
    agent TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (principal, agent)
);
-- What was last said about each agent, by kind, so a restart never says it again.
CREATE TABLE IF NOT EXISTS push_said (
    agent TEXT NOT NULL,
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (agent, kind)
);
