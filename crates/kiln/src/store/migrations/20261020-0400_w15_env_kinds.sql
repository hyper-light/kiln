-- What an environment deploys to and where (docs/ci.md W15-R02, R08): its kind (`kubernetes`,
-- `host`, `container`, `microvm` or `platform`) and its targets (JSON: clusters and namespace,
-- the runners it deploys on, or its integration).
ALTER TABLE ci_environments ADD COLUMN kind TEXT;
ALTER TABLE ci_environments ADD COLUMN targets TEXT NOT NULL DEFAULT '{}';
