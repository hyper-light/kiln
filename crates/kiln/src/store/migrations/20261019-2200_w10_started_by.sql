-- W10-R02's `secrets.use`: who in kiln caused a run, so its legs are given only the secrets they
-- may use. Each commit pushed to a kiln-hosted repository's branches and tags, by whom (the poller
-- makes the push's events later); and whom a diff's version and a land queue entry came from.
CREATE TABLE IF NOT EXISTS ci_pushes (
    repo_id TEXT NOT NULL,
    ref TEXT NOT NULL,
    sha TEXT NOT NULL,
    started_by TEXT NOT NULL,
    ts INTEGER NOT NULL,
    PRIMARY KEY (repo_id, ref, sha)
);
CREATE INDEX IF NOT EXISTS ci_pushes_ts ON ci_pushes (ts);
ALTER TABLE diff_versions ADD COLUMN started_by TEXT;
ALTER TABLE land_queue ADD COLUMN started_by TEXT;
