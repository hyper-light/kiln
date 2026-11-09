-- W05, W13-R01 (docs/needs.md): what finished runs did on their runners, for the learned
-- run-time model (kiln_ci::runtime). Each row is folded into its key's statistics (run_model, the
-- model's sufficient statistics: a fit from them is a fit from every row ever kept) as it's
-- inserted. So rows aren't kept for the fit, and retention isn't an age: each key keeps its newest
-- kiln_ci::runtime::ROWS_KEPT rows (the model's terms and one, the fewest a fit with a residual
-- rests on), to be looked at and to refit from should the terms change. `p` is the probability
-- the row's placement had (Bottou et al. 2013); none for one made with certainty.
CREATE TABLE IF NOT EXISTS run_observations (
  id TEXT PRIMARY KEY,
  runner TEXT NOT NULL,
  key TEXT NOT NULL,
  kind TEXT NOT NULL,
  received_ms INTEGER NOT NULL,
  wall_s REAL NOT NULL,
  p REAL,
  json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS run_observations_by_key ON run_observations (key, received_ms);
-- A row folded in and then let go stays counted once: each runner's ids (UUIDv7, in the order it
-- made them) up to the newest it let go of, per key, are known as seen, and one sent again is
-- refused by that.
CREATE TABLE IF NOT EXISTS run_pruned (
  runner TEXT NOT NULL,
  key TEXT NOT NULL,
  through TEXT NOT NULL,
  PRIMARY KEY (runner, key)
);
CREATE TABLE IF NOT EXISTS run_model (
  key TEXT PRIMARY KEY,
  stats TEXT NOT NULL,
  runs INTEGER NOT NULL,
  updated_ms INTEGER NOT NULL
);
