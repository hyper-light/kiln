-- Steps in flight (docs/ci.md W28-R72): the leases of memo steps being executed, one per step key
-- and trust domain, kept so a restart rebuilds the table (their holders renew them or they lapse).
-- `lease` is `kiln_ci::work::flight::Lease` as JSON.
CREATE TABLE IF NOT EXISTS work_flight (
    id TEXT PRIMARY KEY,
    lease TEXT NOT NULL
) WITHOUT ROWID;
