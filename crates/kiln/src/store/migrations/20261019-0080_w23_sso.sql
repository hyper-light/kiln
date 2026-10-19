-- SSO with GitHub, GitLab and Bitbucket (docs/ci.md W23-R08): a user's email (matched to a host's
-- verified one), the host accounts linked to users (with the host's token, sealed, for acting on
-- the host as the person), and the sign-ins under way.
ALTER TABLE users ADD COLUMN email TEXT;
CREATE INDEX users_email ON users (email);
CREATE TABLE user_identities (
    provider TEXT NOT NULL,
    subject TEXT NOT NULL,
    user_id TEXT NOT NULL,
    login TEXT NOT NULL,
    email TEXT,
    groups TEXT NOT NULL DEFAULT '[]',
    token_sealed BLOB,
    token_nonce BLOB,
    created_at INTEGER NOT NULL,
    last_at INTEGER NOT NULL,
    PRIMARY KEY (provider, subject)
);
CREATE INDEX user_identities_user ON user_identities (user_id);
CREATE TABLE sso_states (
    state TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    return_to TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
