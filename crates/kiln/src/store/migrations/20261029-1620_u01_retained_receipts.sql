-- U01: exact accepted phases outlive pruned capacity tuples; bounded by the writer, no TTL.
CREATE TABLE ci_retained_receipts (
    job TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    agent TEXT NOT NULL,
    execution TEXT NOT NULL,
    released INTEGER NOT NULL,
    until_at INTEGER NOT NULL,
    PRIMARY KEY (job, attempt, agent, execution, released, until_at)
);
-- Existing authenticated ownership is itself durable acceptance evidence.
INSERT INTO ci_retained_receipts (job, attempt, agent, execution, released, until_at)
SELECT job, attempt, agent, execution, 0, until_at FROM ci_retained_ownership WHERE execution IS NOT NULL;
INSERT INTO ci_retained_receipts (job, attempt, agent, execution, released, until_at)
SELECT job, attempt, agent, execution, 1, 0 FROM ci_retained_ownership WHERE execution IS NOT NULL AND released = 1;
