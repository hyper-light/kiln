-- W29-R26: first identities and accepted tag resolutions survive refreshes and restarts.
-- Observations never replace acceptance; only a reviewed change may populate its proof.
CREATE TABLE IF NOT EXISTS ci_lock_sources (
    source TEXT PRIMARY KEY,
    publisher TEXT NOT NULL,
    native INTEGER NOT NULL,
    first_identity TEXT NOT NULL,
    accepted_identity TEXT NOT NULL,
    first_seen INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ci_lock_refs (
    source TEXT NOT NULL REFERENCES ci_lock_sources(source),
    git_ref TEXT NOT NULL,
    first_commit TEXT NOT NULL,
    accepted_commit TEXT NOT NULL,
    last_commit TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    accepted_change TEXT,
    accepted_by TEXT,
    accepted_at INTEGER,
    PRIMARY KEY (source, git_ref)
);
CREATE TABLE IF NOT EXISTS ci_lock_moves (
    id INTEGER PRIMARY KEY,
    source TEXT NOT NULL REFERENCES ci_lock_sources(source),
    git_ref TEXT NOT NULL,
    before_commit TEXT NOT NULL,
    now_commit TEXT NOT NULL,
    identity TEXT NOT NULL,
    before_identity TEXT NOT NULL,
    consumer_repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    workflow TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    notified_at INTEGER,
    UNIQUE (source, git_ref, before_commit, now_commit, identity, consumer_repo_id)
);
CREATE INDEX IF NOT EXISTS ci_lock_moves_pending ON ci_lock_moves(notified_at);
