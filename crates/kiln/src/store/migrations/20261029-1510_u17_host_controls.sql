-- U17: host-affine observations and accepted transfer control metadata share Amphora.
-- The import identity and the canonical value commit together; legacy files are read-only.
CREATE TABLE IF NOT EXISTS host_controls (
    host TEXT NOT NULL,
    kind TEXT NOT NULL,
    version INTEGER NOT NULL,
    source TEXT NOT NULL,
    source_digest TEXT NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (host, kind)
);
