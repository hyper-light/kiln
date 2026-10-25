-- Records (docs/seams.md, "Records") kept in kiln's store: the seam's durable implementation until
-- Amphora's (CP-T1.5) takes its place. Every version of every row, by family, key and the commit
-- that wrote it (a delete's value NULL), each commit's writes in their order; each family's newest
-- change; the head, and how far back snapshots and the change feed go.
CREATE TABLE IF NOT EXISTS records_versions (
    family TEXT NOT NULL,
    key BLOB NOT NULL,
    rev INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    value BLOB,
    PRIMARY KEY (family, key, rev)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS records_versions_feed ON records_versions (family, rev, seq);
CREATE TABLE IF NOT EXISTS records_families (
    family TEXT PRIMARY KEY,
    newest INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS records_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    head INTEGER NOT NULL,
    horizon INTEGER NOT NULL,
    log_horizon INTEGER NOT NULL
);
INSERT OR IGNORE INTO records_meta (id, head, horizon, log_horizon) VALUES (1, 0, 0, 0);
