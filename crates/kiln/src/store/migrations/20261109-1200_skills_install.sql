-- Skills kiln installs for agents (docs/agents.md, "Skills kiln installs"): each package installed,
-- and what a person last looked at to install (one each). A package's whole record is its body
-- (JSON: its source and commit, what it holds, its files by digest, whom it's for); its files'
-- bytes are in the store of bytes (`skills/files/<sha256>`). New tables: an older kiln never names them.
CREATE TABLE skill_packages (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE skill_drafts (
    id TEXT PRIMARY KEY,
    by TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX skill_drafts_by ON skill_drafts (by);
