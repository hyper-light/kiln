-- Labels on pull requests (diffs and change requests), as GitHub's issues API keeps them (docs/ci.md
-- W14-R11): what `actions/labeler` and `create-pull-request` set.
CREATE TABLE diff_labels (
    diff_id TEXT NOT NULL,
    name TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (diff_id, name)
);
