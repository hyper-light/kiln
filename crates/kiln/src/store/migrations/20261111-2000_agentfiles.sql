-- Agentfiles people keep in kiln (`crate::agentfiles`, docs/api.md "Agentfiles"; kiln plan
-- 01a1cd8e862e7169a074db3d5fa33f3d step 30): each its owner's (`owner`, the server's owner, or
-- `user:<id>`), named once among that owner's, its text, and every version of its text (a change of
-- text is a new version, and so is a restore). New tables: an older kiln never names them.
CREATE TABLE agentfiles (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    text TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (owner, name)
);

CREATE TABLE agentfile_versions (
    agentfile_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    text TEXT NOT NULL,
    at INTEGER NOT NULL,
    by TEXT NOT NULL,
    PRIMARY KEY (agentfile_id, version)
);
