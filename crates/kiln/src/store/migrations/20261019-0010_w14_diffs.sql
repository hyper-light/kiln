-- Diffs, their versions, stacks and reviewers (docs/ci.md W14-R03, R13). A diff is one commit
-- under review (`D12`); a change request (`CR4`, W14-R04) shares the table with its own numbers.
CREATE TABLE diffs (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    number INTEGER NOT NULL,
    kind TEXT NOT NULL DEFAULT 'diff',
    change_id TEXT,
    title TEXT NOT NULL,
    summary TEXT NOT NULL DEFAULT '',
    author TEXT NOT NULL,
    author_kind TEXT NOT NULL,
    state TEXT NOT NULL,
    trunk TEXT NOT NULL,
    stack_id TEXT,
    parent_diff TEXT,
    topic TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (kind, number)
);
CREATE INDEX diffs_change ON diffs (repo_id, trunk, change_id);
CREATE INDEX diffs_stack ON diffs (stack_id);
CREATE INDEX diffs_repo ON diffs (repo_id, state);

-- Each pushed version of a diff, kept at refs/kiln/diffs/D<n>/v<k>.
CREATE TABLE diff_versions (
    diff_id TEXT NOT NULL,
    n INTEGER NOT NULL,
    commit_sha TEXT NOT NULL,
    parent TEXT NOT NULL,
    base TEXT NOT NULL,
    patch_id TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT '',
    note TEXT,
    rebase_only INTEGER NOT NULL DEFAULT 0,
    pushed_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (diff_id, n)
);
CREATE INDEX diff_versions_commit ON diff_versions (commit_sha);

CREATE TABLE stacks (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    trunk TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE diff_reviewers (
    diff_id TEXT NOT NULL,
    reviewer TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'not-looked',
    added_at INTEGER NOT NULL,
    PRIMARY KEY (diff_id, reviewer)
);
