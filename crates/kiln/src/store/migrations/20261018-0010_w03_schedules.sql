-- W03-R10: each schedule entry's fires, so a minute fires once whatever restarts, and catch-up
-- knows what was missed.
CREATE TABLE ci_schedule_fires (
  repo_id TEXT NOT NULL,
  path TEXT NOT NULL,                   -- the workflow
  entry INTEGER NOT NULL,               -- its schedule's index
  minute INTEGER NOT NULL,              -- unix seconds of the minute it fired for
  cron TEXT NOT NULL,
  PRIMARY KEY (repo_id, path, entry, minute)
);
