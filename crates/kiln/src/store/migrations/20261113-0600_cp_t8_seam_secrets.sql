-- kiln's store behind kiln-seams' Secrets (CP-T8.3, Shards plan step 33's step 5;
-- crates/kiln/src/secrets/seam.rs): each secret, its versions with their stages, each value
-- sealed under the secrets service's data keys (secret_keys), and the leases on them.
CREATE TABLE IF NOT EXISTS seam_secrets (
    name TEXT PRIMARY KEY,
    -- When it goes for good, once its delete is scheduled.
    purge_at_ms INTEGER
);

CREATE TABLE IF NOT EXISTS seam_secret_versions (
    name TEXT NOT NULL,
    seq INTEGER NOT NULL,
    id TEXT NOT NULL,
    -- Its stages, a letter each: C current, P previous, N pending.
    stages TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    ciphertext BLOB NOT NULL,
    nonce BLOB NOT NULL,
    key_id TEXT NOT NULL,
    PRIMARY KEY (name, seq)
);

CREATE TABLE IF NOT EXISTS seam_leases (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    expires_ms INTEGER NOT NULL
);
