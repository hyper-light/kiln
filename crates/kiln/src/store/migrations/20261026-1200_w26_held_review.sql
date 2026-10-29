-- Comments kiln holds for a host with no batched reviews of its own (Bitbucket's), until their
-- author publishes them together (docs/ci.md W26-R20, docs/seams.md "Forge").
CREATE TABLE IF NOT EXISTS review_held (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo TEXT NOT NULL,
    number INTEGER NOT NULL,
    author TEXT NOT NULL,
    body TEXT NOT NULL,
    path TEXT,
    line INTEGER,
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS review_held_change ON review_held (repo, number, author);
