-- W15 (docs/ci.md), CI's half: environments made in kiln or named by a job (trunk's
-- .kiln/environments.yaml defines the rest), reviews of runs' deployments, and deployments with
-- their statuses (as GitHub's REST API shows them).
CREATE TABLE ci_environments (
  repo_id TEXT NOT NULL,
  name TEXT NOT NULL COLLATE NOCASE,
  protection TEXT NOT NULL DEFAULT '{}',  -- JSON: kiln_ci::env::Protection
  url TEXT,
  source TEXT NOT NULL,                   -- kiln (made in kiln) or run (named by a job)
  created_at INTEGER NOT NULL,
  created_by TEXT NOT NULL,
  PRIMARY KEY (repo_id, name)
);

CREATE TABLE ci_env_reviews (
  id INTEGER PRIMARY KEY,
  run_id TEXT NOT NULL,
  environment TEXT NOT NULL COLLATE NOCASE,
  approved INTEGER NOT NULL,
  comment TEXT NOT NULL DEFAULT '',
  principal TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX ci_env_reviews_run ON ci_env_reviews (run_id);

CREATE TABLE ci_deployments (
  id INTEGER PRIMARY KEY,
  repo_id TEXT NOT NULL,
  environment TEXT NOT NULL COLLATE NOCASE,
  sha TEXT NOT NULL,
  git_ref TEXT NOT NULL,
  task TEXT NOT NULL DEFAULT 'deploy',
  payload TEXT NOT NULL DEFAULT '{}',
  description TEXT,
  creator TEXT NOT NULL,
  run_id TEXT,                            -- the run whose job deployed; none: made through the API
  leg_id TEXT,
  transient INTEGER NOT NULL DEFAULT 0,
  production INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX ci_deployments_env ON ci_deployments (repo_id, environment, id);

CREATE TABLE ci_deployment_statuses (
  id INTEGER PRIMARY KEY,
  deployment_id INTEGER NOT NULL,
  state TEXT NOT NULL,                    -- error, failure, inactive, in_progress, queued, pending, success
  description TEXT,
  environment_url TEXT,
  log_url TEXT,
  creator TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX ci_deployment_statuses_deployment ON ci_deployment_statuses (deployment_id, id);
