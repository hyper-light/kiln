-- W10-R01 to R03 (docs/ci.md): people, their grants, custom roles. Tokens (`tokens`, 0050) gain
-- personal access tokens (`kind = 'pat'`, principal `user:<id>` or `owner`) and agent tokens
-- (`kind = 'agent'`, principal `agent:<label>`, on behalf of `user:<id>` or `owner`), each with its
-- own grant in `scopes` (`{"grants": [...]}`): a token does at most what both it and its user may.
CREATE TABLE users (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE COLLATE NOCASE,
  created_at INTEGER NOT NULL,
  created_by TEXT NOT NULL,
  disabled_at INTEGER,
  -- Part of each sign-in session's signature: raising it signs the user out everywhere.
  session_epoch INTEGER NOT NULL DEFAULT 0,
  -- The one sign-in link that works (its secret's SHA-256), until used or expired.
  link_hash TEXT,
  link_expires_at INTEGER
);

CREATE TABLE roles (                    -- custom roles; the built-in ones live in code
  name TEXT PRIMARY KEY,
  capabilities TEXT NOT NULL,           -- JSON array of capability names
  created_at INTEGER NOT NULL
);

CREATE TABLE grants (
  id INTEGER PRIMARY KEY,
  principal TEXT NOT NULL,              -- user:<id>
  what TEXT NOT NULL,                   -- a role or a capability
  scope TEXT NOT NULL,                  -- server, org:<o>, repo:<o>/<r>, workflow:…, environment:…, runner:…, pool:…
  deny INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  created_by TEXT NOT NULL
);
CREATE INDEX grants_principal ON grants (principal);
