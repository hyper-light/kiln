-- A release asked for again with the same idempotency key is the same release (docs/deploy-api.md
-- §9): an agent, or a script, retrying after a timeout doesn't release twice.
ALTER TABLE deploy_releases ADD COLUMN idempotency_key TEXT;
CREATE UNIQUE INDEX deploy_releases_idempotency ON deploy_releases (app, environment, idempotency_key) WHERE idempotency_key IS NOT NULL;
