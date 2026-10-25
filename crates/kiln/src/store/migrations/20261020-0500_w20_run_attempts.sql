-- Who started each attempt of a run after its first (a rerun): GitHub's `triggering_actor`, and
-- the runner minutes an attached agent's reruns take (docs/ci.md W05-R17, W20-R10).
CREATE TABLE ci_run_attempts (
  run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  attempt INTEGER NOT NULL,
  actor TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (run_id, attempt)
);
