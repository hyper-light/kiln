-- W10-R03 (docs/ci.md §2.4, block 3): tokens, stored as SHA-256 hashes with their last use. Job
-- tokens (`kiln_job_…`, a leg's GITHUB_TOKEN) carry the job's permissions and are revoked when the
-- attempt ends.
CREATE TABLE tokens (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,                   -- job (later: pat, agent, registry)
  principal TEXT NOT NULL,              -- for a job token, its leg
  on_behalf_of TEXT,                    -- for a job token, its run
  hash TEXT NOT NULL UNIQUE,
  scopes TEXT NOT NULL,                 -- JSON: a job token's permissions, and its repository
  created_at INTEGER NOT NULL,
  expires_at INTEGER,
  last_used_at INTEGER,
  revoked_at INTEGER
);
CREATE INDEX tokens_principal ON tokens (kind, principal);
