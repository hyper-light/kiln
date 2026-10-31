-- U10: retain every durable decision while making its protected action explicit. Legacy
-- approvals cover the run's environment-bound jobs, exactly as before, never a release.
ALTER TABLE ci_env_reviews ADD COLUMN protected_action TEXT;
UPDATE ci_env_reviews SET protected_action = 'job:' || run_id;
CREATE INDEX ci_env_reviews_action ON ci_env_reviews (protected_action, environment);
