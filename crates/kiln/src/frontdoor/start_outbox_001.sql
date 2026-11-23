-- W21-R15. Source-only start holds, deliberately outside the replicated admission queue.
CREATE TABLE IF NOT EXISTS starts (
    key TEXT NOT NULL, slot TEXT NOT NULL, target TEXT NOT NULL, digest TEXT NOT NULL,
    payload BLOB, state TEXT NOT NULL, why TEXT NOT NULL, result TEXT,
    UNIQUE(key,slot)
);
CREATE INDEX IF NOT EXISTS start_delivery ON starts(target) WHERE state='held';
CREATE INDEX IF NOT EXISTS start_receipts ON starts(slot) WHERE state='held';
CREATE TABLE IF NOT EXISTS operations (
    key TEXT PRIMARY KEY, digest TEXT NOT NULL, target TEXT NOT NULL, actor TEXT NOT NULL,
    requested_at INTEGER NOT NULL
);

-- An operation owns its child-budget slots across concurrent requests and source restarts.
CREATE TABLE IF NOT EXISTS children (
    actor TEXT NOT NULL, slot TEXT NOT NULL, key TEXT NOT NULL, digest TEXT NOT NULL,
    PRIMARY KEY(actor,slot)
) WITHOUT ROWID;
