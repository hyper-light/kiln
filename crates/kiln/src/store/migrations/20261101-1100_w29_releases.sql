-- Each full version a repository kiln hosts has had (docs/ci.md W29-R21), by push or by Release,
-- with its commit (or tag object), when and by whom: a release's name is never used again, even
-- once its tag is gone. Apart from W14's `ci_releases`, the GitHub-shaped records of the REST
-- facade. A table older builds never read.
CREATE TABLE IF NOT EXISTS ci_released_tags (
    repo_id TEXT NOT NULL,
    tag TEXT NOT NULL,
    sha TEXT NOT NULL,
    at INTEGER NOT NULL,
    by TEXT NOT NULL,
    PRIMARY KEY (repo_id, tag)
);
