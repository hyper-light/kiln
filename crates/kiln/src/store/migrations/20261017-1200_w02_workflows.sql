-- docs/ci.md W02-R11: each workflow the loader read, by repository, path and content (the
-- workflow's blob, and its overlay's when it has one), so an unchanged file is never parsed twice.
CREATE TABLE IF NOT EXISTS ci_workflows (
  id TEXT PRIMARY KEY,
  repo_id TEXT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  content_key TEXT NOT NULL,
  dialect TEXT NOT NULL,
  name TEXT NOT NULL,
  def TEXT,
  diagnostics TEXT NOT NULL DEFAULT '[]',
  created_at INTEGER NOT NULL,
  UNIQUE (repo_id, path, content_key)
);
