-- W24-R31: the reaper's policies (docs/ci.md W24-R31), one per scope and target. A key a policy
-- leaves out comes from another scope or kiln's default (kiln_ci::work::reap::resolve).
CREATE TABLE work_reap_policies (
    -- system, runner, repo or session
    scope TEXT NOT NULL,
    -- '' for the system; the runner's name, the repository's (owner/name), or the session's id
    target TEXT NOT NULL,
    -- kiln_ci::work::reap::Policy as JSON
    policy TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    PRIMARY KEY (scope, target)
);
