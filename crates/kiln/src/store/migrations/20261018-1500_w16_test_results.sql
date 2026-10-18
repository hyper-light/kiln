-- Tests and their results (docs/ci.md W16-R02). A case is a repository's test, by the hash of its
-- suite, name and file; a result is one run of it in a leg's attempt; a report is a file a step
-- sent (kept in the CAS, parsed here).
CREATE TABLE IF NOT EXISTS test_cases (
    repo_id TEXT NOT NULL,
    id TEXT NOT NULL,
    suite TEXT NOT NULL,
    name TEXT NOT NULL,
    file TEXT,
    first_seen INTEGER NOT NULL,
    PRIMARY KEY (repo_id, id)
);
CREATE TABLE IF NOT EXISTS test_reports (
    id INTEGER PRIMARY KEY,
    leg TEXT NOT NULL,
    step_idx INTEGER,
    path TEXT NOT NULL,
    format TEXT NOT NULL,
    object TEXT NOT NULL,
    cases INTEGER NOT NULL,
    failed INTEGER NOT NULL,
    ts INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS test_reports_leg ON test_reports (leg);
CREATE TABLE IF NOT EXISTS test_results (
    id INTEGER PRIMARY KEY,
    repo_id TEXT NOT NULL,
    case_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    leg TEXT NOT NULL,
    -- The leg's key in its run, the same across attempts: with the commit, what its inputs are.
    leg_key TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    branch TEXT NOT NULL,
    sha TEXT NOT NULL,
    report INTEGER,
    outcome TEXT NOT NULL,
    duration_ms INTEGER,
    message TEXT,
    trace TEXT,
    line INTEGER,
    retried INTEGER NOT NULL DEFAULT 0,
    quarantined INTEGER NOT NULL DEFAULT 0,
    ts INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS test_results_case ON test_results (repo_id, case_id, ts);
CREATE INDEX IF NOT EXISTS test_results_leg ON test_results (leg);
CREATE INDEX IF NOT EXISTS test_results_run ON test_results (run_id);
