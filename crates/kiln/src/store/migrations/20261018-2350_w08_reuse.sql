-- Job-result reuse (docs/ci.md W08-R10): legs that succeeded, by their input hash, with the
-- report a leg that takes their result over gives its plan.
CREATE TABLE ci_reuse (
    input_hash TEXT NOT NULL,
    leg_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    report TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (input_hash, leg_id)
);
CREATE INDEX ci_reuse_hash ON ci_reuse (input_hash, created_at);
