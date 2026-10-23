-- Diffs to land as soon as their trunk's policy lets them (docs/ci.md W14-R11: GitHub's auto-merge,
-- as a branch that requires a merge queue has it, which every kiln trunk is): into the land queue
-- then, with the strategy asked, as whoever asked. A diff's row goes once it's in the queue.
CREATE TABLE land_when_ready (
  diff_id TEXT PRIMARY KEY,
  strategy TEXT,                          -- the one asked, else the policy's
  enabled_by TEXT NOT NULL,               -- as the audit names it: `job:<leg>`, `user:<name>`
  started_by TEXT,                        -- what its candidate's runs are held to (as land_queue's)
  enabled_at INTEGER NOT NULL,
  commit_title TEXT,
  commit_message TEXT
);
