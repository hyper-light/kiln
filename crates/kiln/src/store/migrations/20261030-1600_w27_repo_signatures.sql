-- Whether a repository kiln hosts requires the commits a push brings to be signed, where its
-- .kiln/review.yaml doesn't say (docs/ci.md W27-R03): `required`, or `warn` (taken, each commit
-- that doesn't verify named in the push's report). Those from before warn; a new one takes the
-- server's `[ci] signatures` as it's made.
ALTER TABLE repos ADD COLUMN signatures TEXT NOT NULL DEFAULT 'warn';
