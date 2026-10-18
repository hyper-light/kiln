-- Outside contributors' runs (docs/ci.md W18-R06): `required` while the run waits for a
-- maintainer before any job starts, then `approved` or `rejected`, with who and when.
ALTER TABLE runs ADD COLUMN approval TEXT;
ALTER TABLE runs ADD COLUMN approved_by TEXT;
ALTER TABLE runs ADD COLUMN approved_at INTEGER;
