-- The templates apps are made from (docs/deploy-api.md §3.2), each kept as its JSON.
CREATE TABLE deploy_templates (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
