-- W05-R26 (docs/needs.md "Pace"): the fleet's per-core speeds, learned from what finished runs
-- did. Each run that did its kind's whole work is folded, as it's kept in run_observations, into
-- its kind's cell on its runner: its count, and the sum and sum of squares of its ln CPU-seconds
-- (kiln_ci::pace::speed::Cell). A fit from the cells is the fit from every run ever kept, while
-- run_observations keeps only each key's newest rows. `key` is the kind as every machine knows it
-- (kiln_ci::pace::speed::key_of: a CI leg's key, a command's work key).
CREATE TABLE IF NOT EXISTS run_speed_cells (
  key TEXT NOT NULL,
  runner TEXT NOT NULL,
  n INTEGER NOT NULL,
  sum REAL NOT NULL,
  sq REAL NOT NULL,
  PRIMARY KEY (key, runner)
);
