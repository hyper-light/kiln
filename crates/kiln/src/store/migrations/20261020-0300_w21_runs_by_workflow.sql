-- A workflow's runs by number, for how long its jobs typically take on the run graph
-- (docs/ci.md W21-R04).
CREATE INDEX runs_by_workflow ON runs (repo_id, workflow_id, number) WHERE workflow_id IS NOT NULL;
