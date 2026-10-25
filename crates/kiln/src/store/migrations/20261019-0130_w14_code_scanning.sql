-- Code scanning on the facade (docs/ci.md W14-R11): the SARIF analyses uploaded (gzipped, in
-- base64, as sent), and the alerts read from them, one per fingerprint on a ref.
CREATE TABLE ci_code_scanning_analyses (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id TEXT NOT NULL,
    sarif_id TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    git_ref TEXT NOT NULL,
    category TEXT NOT NULL DEFAULT '',
    tool TEXT NOT NULL,
    tool_version TEXT,
    results INTEGER NOT NULL,
    rules INTEGER NOT NULL,
    sarif BLOB NOT NULL,
    leg_id TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX ci_code_scanning_analyses_repo ON ci_code_scanning_analyses (repo_id, git_ref, tool, created_at);
CREATE INDEX ci_code_scanning_analyses_sarif ON ci_code_scanning_analyses (repo_id, sarif_id);
CREATE TABLE ci_code_scanning_alerts (
    repo_id TEXT NOT NULL,
    number INTEGER NOT NULL,
    git_ref TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    tool TEXT NOT NULL,
    category TEXT NOT NULL DEFAULT '',
    rule_id TEXT NOT NULL,
    rule_name TEXT,
    rule_description TEXT,
    severity TEXT NOT NULL,
    security_severity TEXT,
    message TEXT NOT NULL,
    path TEXT NOT NULL,
    start_line INTEGER,
    end_line INTEGER,
    start_column INTEGER,
    end_column INTEGER,
    commit_sha TEXT NOT NULL,
    state TEXT NOT NULL,
    first_analysis INTEGER NOT NULL,
    last_analysis INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    fixed_at INTEGER,
    PRIMARY KEY (repo_id, number)
);
CREATE UNIQUE INDEX ci_code_scanning_alerts_fingerprint ON ci_code_scanning_alerts (repo_id, git_ref, fingerprint);
