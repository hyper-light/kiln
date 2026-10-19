-- An environment's concurrency limit (docs/ci.md W15-R01): at most this many of its deployments
-- (legs bound to it, past its protection) at once; NULL for no limit.
ALTER TABLE ci_environments ADD COLUMN concurrency INTEGER;
