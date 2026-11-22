-- Scale & query performance indexes for 10,000+ agent sessions (Mission Control attention)
-- and Ship commit/deployment queries.
CREATE INDEX IF NOT EXISTS attention_requests_open ON attention_requests (closed_at, agent, observed_at);
CREATE INDEX IF NOT EXISTS attention_requests_row ON attention_requests (row_agent, closed_at);
CREATE INDEX IF NOT EXISTS attention_responses_principal_at ON attention_responses (principal, at DESC);
CREATE INDEX IF NOT EXISTS attention_response_attempts_principal_at ON attention_response_attempts (principal, at DESC);
CREATE INDEX IF NOT EXISTS attention_choices_principal_at ON attention_choices (principal, at DESC);
CREATE INDEX IF NOT EXISTS attention_visits_principal_at ON attention_visits (principal, at DESC);
CREATE INDEX IF NOT EXISTS attention_exposures_principal_at ON attention_exposures (principal, at DESC);
CREATE INDEX IF NOT EXISTS attention_archive_principal_at ON attention_archive (principal, at DESC);
CREATE INDEX IF NOT EXISTS attention_uses_agent_kind ON attention_uses (agent, kind, edge, at DESC);
CREATE INDEX IF NOT EXISTS ship_order_repo_seq ON ship_order (repo_id, seq DESC);
CREATE INDEX IF NOT EXISTS ci_deployments_repo_sha ON ci_deployments (repo_id, sha, id DESC);
CREATE INDEX IF NOT EXISTS ci_images_run ON ci_images (run_id, id);
CREATE INDEX IF NOT EXISTS deploy_releases_env_state ON deploy_releases (lower(environment), state, id);
