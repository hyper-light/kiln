-- W24's work ledger (docs/ci.md W24-R02): what units of work produced, by family and key, shaped
-- as the Records seam's rows so the ledger can move onto Records (Amphora) as it is. Families:
-- `work.result` (a strong key's result), `work.weak` (a weak key's path sets, its newest 8 kept),
-- `work.pathset` (a path set, by its digest), `work.snapshot` (a warm workspace's snapshot) and
-- `work.lane` (a lane's newest snapshot). Keys start with their scope, `owner/repo|domain|`; `at`
-- orders a weak key's path sets, newest highest.
CREATE TABLE IF NOT EXISTS work_records (
    family TEXT NOT NULL,
    key TEXT NOT NULL,
    value BLOB NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (family, key)
) WITHOUT ROWID;
