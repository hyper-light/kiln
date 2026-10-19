-- Runner images kiln built (docs/ci.md W13-R11): `kiln/build-image@v1`'s reports, each image in
-- kiln's registry with its platforms, for pools and routing rules.
CREATE TABLE ci_images (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id TEXT NOT NULL,
    name TEXT NOT NULL,
    reference TEXT NOT NULL,
    digest TEXT NOT NULL,
    platforms TEXT NOT NULL DEFAULT '[]',
    run_id TEXT,
    leg_id TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX ci_images_name ON ci_images (repo_id, name, created_at);
