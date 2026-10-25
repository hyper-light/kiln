-- The GitHub App (docs/ci.md W14-R10, W23): the one GitHub's manifest flow made (its private key
-- and webhook secret sealed by the master key), the manifest flow's pending states, and the check
-- run kept on GitHub for each KilnCI run of a repository there.
CREATE TABLE github_app (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    app_id INTEGER NOT NULL,
    slug TEXT,
    client_id TEXT,
    html_url TEXT,
    key_sealed BLOB NOT NULL,
    key_nonce BLOB NOT NULL,
    secret_sealed BLOB,
    secret_nonce BLOB,
    client_secret_sealed BLOB,
    client_secret_nonce BLOB,
    created_at INTEGER NOT NULL
);

CREATE TABLE github_manifests (
    state TEXT PRIMARY KEY,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE github_checks (
    run_id TEXT PRIMARY KEY,
    repo TEXT NOT NULL,
    check_id INTEGER NOT NULL,
    state TEXT NOT NULL,
    attempt INTEGER NOT NULL DEFAULT 1,
    updated_at INTEGER NOT NULL
);
