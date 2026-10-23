-- What each person did with a notice (docs/notifications.md, "Put away, acted on, seen"): a
-- request or card put away (`away`) or acted on (`done`), or an agent seen (`seen`, the latest
-- kept), so no device, browser or kiln address shows it again, and no push says it again.
CREATE TABLE IF NOT EXISTS notice_marks (
    principal TEXT NOT NULL,
    key TEXT NOT NULL,
    how TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (principal, key)
);
CREATE INDEX IF NOT EXISTS notice_marks_at ON notice_marks (principal, at);
