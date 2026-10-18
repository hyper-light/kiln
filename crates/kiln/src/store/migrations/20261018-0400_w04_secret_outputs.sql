-- docs/ci.md W04-R22, W09-R10: job outputs declared `secret: true`, sealed as secrets are (the
-- master key, bound to the leg and the name) and deleted with their run. Consumers get them only
-- as masked environment.
CREATE TABLE ci_secret_outputs (
  run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  leg_id TEXT NOT NULL,
  name TEXT NOT NULL,
  sealed BLOB NOT NULL,
  nonce BLOB NOT NULL,
  PRIMARY KEY (leg_id, name)
);
