-- docs/ci.md W05-R17: a rerun is a new attempt of the same run. Each attempt has legs of its
-- own: those not run again are copies whose `reused_from` names the leg whose result they keep.
-- The plan log is kept per attempt, so a restart rebuilds the current one.
ALTER TABLE jobs ADD COLUMN run_attempt INTEGER NOT NULL DEFAULT 1;
ALTER TABLE ci_plan_log ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;
CREATE INDEX IF NOT EXISTS jobs_run_attempt ON jobs (run_id, run_attempt);
