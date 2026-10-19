-- Change requests (docs/ci.md W14-R04): the branch a change request reviews.
ALTER TABLE diffs ADD COLUMN branch TEXT;
CREATE INDEX diffs_branch ON diffs (repo_id, branch);
