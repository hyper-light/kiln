-- W11-R01 (docs/ci.md; docs/cd-targets.md §6, §7.1): integrations, shared by jobs and deployments.
-- A credential an integration holds (a token, a password) is a W09 secret in scope
-- ('integration', <id>), named TOKEN; `secret_ref` names it when there is one.
CREATE TABLE ci_integrations (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  name TEXT NOT NULL UNIQUE,
  config TEXT NOT NULL,                 -- JSON, never secret
  secret_ref TEXT,
  scope_kind TEXT NOT NULL,             -- who may use it: server, organization, repository, environment
  scope_id TEXT NOT NULL,
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  verified_at INTEGER,
  last_error TEXT,
  last_used_at INTEGER
);
