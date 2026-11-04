-- The names machines' certificates carried before kiln knew them by their machine ids
-- (docs/machines.md, "One record a machine"): a runner whose certificate names it so is that
-- machine, keyed by its id from the moment it connects. A new table older builds never name: no
-- new epoch.
CREATE TABLE IF NOT EXISTS machine_names (
    name TEXT PRIMARY KEY,
    machine_id TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS machine_names_id ON machine_names (machine_id);
