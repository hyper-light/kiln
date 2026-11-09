-- Pipelines' drafts (docs/ci.md W29-R10, R11): a workflow or action written and not yet published,
-- one row each. `kind`: `diff`, a draft diff of a repository kiln hosts (`change`, its Kiln-Change,
-- which a later save names to be a new version of it); or `change`, a host's draft pull or merge
-- request from `branch` (`kiln/draft/<stem>-<id8>`), whose `head` is the commit kiln saved last.
-- `state`: draft, review, landed or discarded.
CREATE TABLE ci_pipeline_drafts (
    id TEXT PRIMARY KEY,
    repo TEXT NOT NULL,
    path TEXT NOT NULL,
    author TEXT NOT NULL,
    kind TEXT NOT NULL,
    branch TEXT,
    number INTEGER,
    name TEXT,
    url TEXT,
    head TEXT,
    change TEXT,
    title TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX ci_pipeline_drafts_by_file ON ci_pipeline_drafts (repo, path, author, state);
