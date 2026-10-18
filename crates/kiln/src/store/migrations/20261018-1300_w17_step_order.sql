-- The order a leg's steps started in (docs/ci.md W17-R06, W21): steps that start within the same
-- second, a job container's and the first step's say, keep it.
ALTER TABLE steps ADD COLUMN started_seq INTEGER;
