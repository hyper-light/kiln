-- What each run pinned of other repositories as it started (docs/ci.md W29-R20): an action or a
-- remote reusable workflow, by the run's repository, for the catalog's consumers. `publisher` is
-- `owner/repo` in lower case; `path` an action's directory or a workflow's file, as `uses:` names
-- it; `git_ref` what it wrote after `@`. A table older builds never read.
CREATE TABLE IF NOT EXISTS ci_uses (
    run_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    publisher TEXT NOT NULL,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    git_ref TEXT NOT NULL,
    sha TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (run_id, publisher, path, sha)
);
CREATE INDEX IF NOT EXISTS ci_uses_by_publisher ON ci_uses (publisher, path, at);
