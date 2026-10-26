-- What a repository of kiln's registry keeps (docs/deploy-api.md §3.1, step 5): its tags immutable
-- (a release's: a tag once pushed never points elsewhere, and isn't deleted), and how many tags it
-- keeps (a snapshot path's: the newest; the collector drops the rest).
CREATE TABLE IF NOT EXISTS registry_policies (
    repo TEXT PRIMARY KEY,
    immutable INTEGER NOT NULL DEFAULT 0,
    keep INTEGER,
    updated_at INTEGER NOT NULL
);
