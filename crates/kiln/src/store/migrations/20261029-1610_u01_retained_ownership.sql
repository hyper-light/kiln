-- U01: terminal plans do not end physical ownership. No expiry releases these records.
CREATE TABLE ci_retained_ownership (
    job TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    agent TEXT NOT NULL,
    execution TEXT,
    until_at INTEGER NOT NULL,
    allocation TEXT NOT NULL,
    transferred INTEGER NOT NULL DEFAULT 0,
    released INTEGER NOT NULL DEFAULT 0,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (job, attempt)
);
CREATE INDEX ci_retained_host ON ci_retained_ownership (agent, released);
