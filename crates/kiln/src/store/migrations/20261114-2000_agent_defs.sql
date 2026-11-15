-- Agent definitions people keep in kiln (`crate::agentdefs`, docs/api.md "Agent definitions"; kiln
-- plan 01a1cd8e862e7169a074db3d5fa33f3d P5.1): each its owner's (`owner`, the server's owner, or
-- `user:<id>`), named once among that owner's, and every version of its settings (one of kiln's
-- harnesses, its model, instructions, tools and MCP servers, skills, limits and Agentfile) as JSON.
-- A change of settings is a new version, and so is a restore; a rename alone is none. New tables: an
-- older kiln never names them.
CREATE TABLE agent_defs (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    settings TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (owner, name)
);

CREATE TABLE agent_def_versions (
    agent_def_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    settings TEXT NOT NULL,
    at INTEGER NOT NULL,
    by TEXT NOT NULL,
    PRIMARY KEY (agent_def_id, version)
);
