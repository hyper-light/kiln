-- The chunks each repository's cache entries hold (docs/ci.md W08-R09): what `kiln/cache`'s
-- uploads may skip. A chunk only another repository stored is asked for again, so the store's
-- dedup never tells one repository what another holds.
CREATE TABLE ci_cache_chunks (
    repo_id TEXT NOT NULL,
    digest TEXT NOT NULL,
    PRIMARY KEY (repo_id, digest)
);
