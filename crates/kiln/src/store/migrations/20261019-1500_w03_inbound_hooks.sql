-- Inbound webhooks (docs/ci.md W03-R02): `POST /api/ci/hooks/:id`, signed with a secret derived
-- from the server's master key and the hook's id (never stored), raises kiln's `webhook` event in
-- its repository.
CREATE TABLE ci_hooks (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    name TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_called_at INTEGER,
    calls INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX ci_hooks_repo ON ci_hooks (repo_id);
