-- W14-R11: what jobs record through the facade's REST API: commit statuses, check runs and
-- releases with their assets (content in the store, W08).
CREATE TABLE ci_statuses (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id TEXT NOT NULL,
  sha TEXT NOT NULL,
  state TEXT NOT NULL,                  -- error | failure | pending | success
  context TEXT NOT NULL,
  description TEXT,
  target_url TEXT,
  creator TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX ci_statuses_sha ON ci_statuses (repo_id, sha);
CREATE TABLE ci_check_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id TEXT NOT NULL,
  head_sha TEXT NOT NULL,
  name TEXT NOT NULL,
  status TEXT NOT NULL,                 -- queued | in_progress | completed
  conclusion TEXT,
  details_url TEXT,
  external_id TEXT,
  output TEXT NOT NULL DEFAULT '{}',    -- {title, summary, text, annotations_count}
  started_at INTEGER,
  completed_at INTEGER,
  creator TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX ci_check_runs_sha ON ci_check_runs (repo_id, head_sha);
CREATE TABLE ci_releases (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id TEXT NOT NULL,
  tag_name TEXT NOT NULL,
  target_commitish TEXT NOT NULL,
  name TEXT,
  body TEXT,
  draft INTEGER NOT NULL DEFAULT 0,
  prerelease INTEGER NOT NULL DEFAULT 0,
  author TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  published_at INTEGER,
  UNIQUE (repo_id, tag_name)
);
CREATE TABLE ci_release_assets (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  release_id INTEGER NOT NULL REFERENCES ci_releases(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  label TEXT,
  content_type TEXT NOT NULL,
  size INTEGER NOT NULL,
  object TEXT NOT NULL,                 -- cas_objects.digest
  uploader TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  downloads INTEGER NOT NULL DEFAULT 0,
  UNIQUE (release_id, name)
);
