-- CI providers (docs/ci.md W23): what each repository's CI runs on (KilnCI unless it says), and the
-- runs and jobs kiln keeps of other providers', with their ids and pages there.
CREATE TABLE repo_ci (
    repo_id TEXT PRIMARY KEY,
    providers TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
ALTER TABLE runs ADD COLUMN provider TEXT NOT NULL DEFAULT 'kiln';
ALTER TABLE runs ADD COLUMN external_id TEXT;
ALTER TABLE runs ADD COLUMN external_url TEXT;
ALTER TABLE runs ADD COLUMN external_number INTEGER;
CREATE INDEX runs_external ON runs (repo_id, provider, external_id);
ALTER TABLE jobs ADD COLUMN external_id TEXT;
ALTER TABLE jobs ADD COLUMN external_url TEXT;
