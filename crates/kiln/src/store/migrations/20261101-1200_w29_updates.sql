-- Updates due to consumers (docs/ci.md W29-R22): a release of `publisher` (`owner/repo`, lower
-- case) at `tag`, for repository `repo_id`, which uses an older one and asked for updates. It
-- opens at `due_at`, after the consumer's cooldown; `state` is `pending`, then `opened` (with
-- the drafts it went in) or `skipped` (with why). A table older builds never read.
CREATE TABLE IF NOT EXISTS ci_pending_updates (
    repo_id TEXT NOT NULL,
    publisher TEXT NOT NULL,
    tag TEXT NOT NULL,
    due_at INTEGER NOT NULL,
    state TEXT NOT NULL,
    note TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, publisher, tag)
);
CREATE INDEX IF NOT EXISTS ci_pending_updates_due ON ci_pending_updates (state, due_at);
