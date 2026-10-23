-- docs/ci.md W19-R06: the `inbox` channel, kiln's own list of notifications for each person.
CREATE TABLE IF NOT EXISTS ci_inbox (
  id TEXT PRIMARY KEY,
  person TEXT NOT NULL,            -- user:ada, or admin
  event TEXT NOT NULL,             -- run.failed, review.requested, …
  title TEXT NOT NULL,
  line TEXT NOT NULL,
  url TEXT NOT NULL,
  repo TEXT,                       -- the repository's name, when it's about one
  created_at INTEGER NOT NULL,
  read_at INTEGER
);
CREATE INDEX IF NOT EXISTS ci_inbox_person ON ci_inbox (person, created_at);
