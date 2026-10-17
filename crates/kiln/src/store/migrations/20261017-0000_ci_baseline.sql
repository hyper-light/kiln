-- The migrations table itself; the first migration, so the list is never empty.
CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);
