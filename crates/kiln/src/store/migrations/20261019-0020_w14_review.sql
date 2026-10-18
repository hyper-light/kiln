-- Review and landing (docs/ci.md W14-R05, R07, R08, R13): threads anchored to a version, file,
-- side and lines; their comments (drafts until a review publishes them); reviews; files viewed;
-- the land queue; and where a diff landed.
CREATE TABLE review_threads (
    id TEXT PRIMARY KEY,
    diff_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    file TEXT,
    side TEXT,
    start_line INTEGER,
    end_line INTEGER,
    state TEXT NOT NULL DEFAULT 'open',
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX review_threads_diff ON review_threads (diff_id);

CREATE TABLE review_comments (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    author TEXT NOT NULL,
    body TEXT NOT NULL,
    suggestion TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    published INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX review_comments_thread ON review_comments (thread_id);

CREATE TABLE reviews (
    id TEXT PRIMARY KEY,
    diff_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    reviewer TEXT NOT NULL,
    reviewer_kind TEXT NOT NULL,
    action TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE INDEX reviews_diff ON reviews (diff_id);

CREATE TABLE viewed_files (
    diff_id TEXT NOT NULL,
    user TEXT NOT NULL,
    file TEXT NOT NULL,
    version INTEGER NOT NULL,
    blob TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (diff_id, user, file)
);

ALTER TABLE diffs ADD COLUMN landed_commit TEXT;
ALTER TABLE diffs ADD COLUMN landed_at INTEGER;

CREATE TABLE land_queue (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    trunk TEXT NOT NULL,
    diffs TEXT NOT NULL,
    strategy TEXT NOT NULL,
    state TEXT NOT NULL,
    batch TEXT,
    base TEXT,
    candidate TEXT,
    checks TEXT,
    commits TEXT,
    reason TEXT,
    enqueued_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX land_queue_trunk ON land_queue (repo_id, trunk, state);
