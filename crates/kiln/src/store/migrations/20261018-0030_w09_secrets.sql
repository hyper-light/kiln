-- W09 (docs/ci.md §2.4, block 3): secrets (encrypted), their uses, variables, and the data keys
-- that encrypt secrets (each wrapped by the master key file).
CREATE TABLE ci_keys (
  id TEXT PRIMARY KEY,
  wrapped BLOB NOT NULL,
  nonce BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  retired_at INTEGER
);
CREATE TABLE ci_secrets (
  id TEXT PRIMARY KEY,
  scope_kind TEXT NOT NULL,              -- server | organization | repository | workflow | environment
  scope_id TEXT NOT NULL,                -- '' | owner | repos.id | '<repos.id>:<path>' | '<repos.id>:<environment>'
  name TEXT NOT NULL,                    -- upper-case
  ciphertext BLOB NOT NULL,              -- AES-256-GCM, tag appended; bound to scope and name
  nonce BLOB NOT NULL,
  key_id TEXT NOT NULL REFERENCES ci_keys(id),
  size INTEGER NOT NULL,
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  last_used_at INTEGER,
  UNIQUE (scope_kind, scope_id, name)
);
CREATE TABLE ci_secret_uses (
  secret_id TEXT NOT NULL,
  leg_id TEXT NOT NULL,
  ts INTEGER NOT NULL,
  PRIMARY KEY (secret_id, leg_id)
);
CREATE TABLE ci_variables (
  scope_kind TEXT NOT NULL,
  scope_id TEXT NOT NULL,
  name TEXT NOT NULL,
  value TEXT NOT NULL,
  updated_by TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (scope_kind, scope_id, name)
);
