-- W08 (docs/ci.md §2.4, block 2): the content-addressed store, GitHub's caches and artifacts.

-- An object: its chunks in order, each [digest, size] (JSON), so a range is found without a
-- lookup per chunk. What refers to an object (a cache entry, an artifact) is found when the
-- store is collected; objects carry no reference counts.
CREATE TABLE cas_objects (
  digest TEXT PRIMARY KEY,              -- SHA-256 of the whole content
  size INTEGER NOT NULL,
  chunks TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER NOT NULL
);

-- Each chunk stored once, at <data>/cas/chunks/<2 hex>/<2 hex>/<digest>.
CREATE TABLE cas_chunks (
  digest TEXT PRIMARY KEY,
  size INTEGER NOT NULL
);

-- A cache entry: immutable once its object is set; NULL while it uploads.
CREATE TABLE ci_caches (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id TEXT NOT NULL,
  scope TEXT NOT NULL,                  -- the ref it was saved from
  key TEXT NOT NULL COLLATE NOCASE,
  version TEXT NOT NULL,
  object TEXT,
  size INTEGER,
  created_at INTEGER NOT NULL,
  last_used_at INTEGER,
  created_by_leg TEXT,
  UNIQUE (repo_id, scope, key, version)
);
CREATE INDEX ci_caches_lookup ON ci_caches (repo_id, scope, version);

-- An artifact of a run: unique by name, immutable once finalized.
CREATE TABLE ci_artifacts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  leg_id TEXT,
  name TEXT NOT NULL,
  object TEXT,
  size INTEGER,
  digest TEXT,                          -- sha256:<hex> of the content, as the toolkit reports it
  mime_type TEXT,
  created_at INTEGER NOT NULL,
  expires_at INTEGER,
  finalized INTEGER NOT NULL DEFAULT 0,
  UNIQUE (run_id, name)
);
