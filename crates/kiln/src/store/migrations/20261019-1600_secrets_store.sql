-- The secrets service's own backend (docs/deploy-api.md §3.5): versions of each secret of a
-- scope, sealed under data keys of its own (wrapped by the server's master key), and each read
-- of their values for a delivery.
CREATE TABLE IF NOT EXISTS secret_keys (
    id TEXT PRIMARY KEY,
    wrapped BLOB NOT NULL,
    nonce BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    retired_at INTEGER
);

CREATE TABLE IF NOT EXISTS secret_versions (
    scope TEXT NOT NULL,
    name TEXT NOT NULL,
    version INTEGER NOT NULL,
    -- NULL once destroyed (older than the versions kept), and for a deletion.
    ciphertext BLOB,
    nonce BLOB,
    key_id TEXT,
    size INTEGER NOT NULL DEFAULT 0,
    set_by TEXT NOT NULL,
    set_at INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (scope, name, version)
);

CREATE TABLE IF NOT EXISTS secret_reads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    by_whom TEXT NOT NULL,
    purpose TEXT NOT NULL,
    versions TEXT NOT NULL,
    at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS secret_reads_scope ON secret_reads (scope, id);
