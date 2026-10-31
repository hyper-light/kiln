-- kiln:epoch 2
-- U09: CI retains identity/delivery metadata, values belong to secret_versions.
-- Legacy ciphertext remains readable until transactionally re-sealed at first use.
ALTER TABLE ci_secrets RENAME TO ci_secrets_u09_import;
CREATE TABLE ci_secrets (
    id TEXT PRIMARY KEY,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    name TEXT NOT NULL,
    ciphertext BLOB,
    nonce BLOB,
    key_id TEXT,
    size INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_used_at INTEGER,
    broker TEXT,
    UNIQUE (scope_kind, scope_id, name)
);
INSERT INTO ci_secrets SELECT id, scope_kind, scope_id, name, ciphertext, nonce, key_id,
    size, created_by, created_at, updated_at, last_used_at, broker FROM ci_secrets_u09_import;
DROP TABLE ci_secrets_u09_import;
ALTER TABLE secret_versions ADD COLUMN broker TEXT;
