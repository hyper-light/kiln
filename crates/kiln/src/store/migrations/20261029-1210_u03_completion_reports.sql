-- U03: an authenticated runner result is kept before Received lets the runner discard it.
-- This is workflow lifecycle state in the canonical store, not another delivery controller.
CREATE TABLE ci_completion_reports (
  job TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  attempt INTEGER NOT NULL,
  agent TEXT NOT NULL,
  payload TEXT NOT NULL,
  received_at INTEGER NOT NULL,
  PRIMARY KEY (job, attempt, agent)
) WITHOUT ROWID;
