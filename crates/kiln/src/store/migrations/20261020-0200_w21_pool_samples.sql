-- Each pool's size and the legs waiting for it, a minute apart, for its scaling history
-- (docs/ci.md W21-R08). Kept a week.
CREATE TABLE ci_pool_samples (
    pool TEXT NOT NULL,
    ts INTEGER NOT NULL,
    starting INTEGER NOT NULL,
    idle INTEGER NOT NULL,
    busy INTEGER NOT NULL,
    waiting INTEGER NOT NULL,
    PRIMARY KEY (pool, ts)
);
CREATE INDEX ci_pool_samples_ts ON ci_pool_samples (ts);
