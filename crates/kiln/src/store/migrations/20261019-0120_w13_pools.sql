-- Pools made on the Pools page (docs/ci.md W13-R09): each one's settings as `[[ci.pools]]` has
-- them (JSON), beside the server configuration's, which come first.
CREATE TABLE ci_pools (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
