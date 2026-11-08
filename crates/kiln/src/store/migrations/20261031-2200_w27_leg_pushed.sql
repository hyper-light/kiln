-- What a leg's token pushed to kiln's registry (docs/ci.md W27-R08; the Transfers plan's step 11):
-- each manifest put, by its repository and digest, as the attempt that put it, so the leg's
-- hand-on of what it made names the image and kiln's receipt of that takes custody of it.
CREATE TABLE ci_leg_pushed (
  job_id TEXT NOT NULL,
  attempt INTEGER NOT NULL,
  repo TEXT NOT NULL,
  digest TEXT NOT NULL,
  pushed_at INTEGER NOT NULL,
  PRIMARY KEY (job_id, attempt, repo, digest)
);
