-- Previews (docs/ci.md W15-R07): an environment made from one of `.kiln/environments.yaml`'s
-- `previews:` for a diff, a stack or a pull request, named `<template>-<what it's for>`
-- (`preview-D12`), and what's left to take down where it was deployed once it goes.
CREATE TABLE ci_previews (
  repo_id TEXT NOT NULL,
  name TEXT NOT NULL COLLATE NOCASE,      -- the environment: preview-D12
  template TEXT NOT NULL COLLATE NOCASE,  -- its `previews:` entry
  change TEXT NOT NULL,                   -- what it's for: D12, S<stack id>, CR7, PR42
  shown TEXT NOT NULL,                    -- the same, as people read it: D12, CR7, #42
  kind TEXT,                              -- its template's kind when it was last deployed
  url TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,            -- its last deployment, or when it was made
  expires_at INTEGER NOT NULL,            -- removed then, unless deployed again
  closed_at INTEGER,                      -- removed: kept an hour for its change's closing runs
  closed_why TEXT,                        -- D12 landed, #42 was closed, it expired
  PRIMARY KEY (repo_id, name)
);
CREATE INDEX ci_previews_change ON ci_previews (repo_id, change);

-- A removed preview's deployment on a runner (its Compose project or microVM), taken down when the
-- runner says it has: asked again each time it connects until then.
CREATE TABLE ci_preview_removals (
  runner TEXT NOT NULL,
  kind TEXT NOT NULL,                     -- container or microvm
  repo TEXT NOT NULL,                     -- owner/name, as the runner names deployments
  environment TEXT NOT NULL COLLATE NOCASE,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (runner, kind, repo, environment)
);
