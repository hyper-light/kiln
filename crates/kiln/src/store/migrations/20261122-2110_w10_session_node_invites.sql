-- W10. An owner-issued session node invite binds exactly one enrollment identity.
CREATE TABLE session_node_invites (
    id TEXT PRIMARY KEY,
    public_key BLOB NOT NULL,
    expires_at INTEGER NOT NULL,
    holder TEXT
);
CREATE INDEX session_node_invites_expiry ON session_node_invites(expires_at);
