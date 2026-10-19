-- Deploying by API (docs/deploy-api.md §14.1): environments, apps, registrations and releases,
-- each kept as its JSON with the columns queries need, and the releases being followed (the work
-- queue that Focal's bus takes over, §14.2), each leased by the server that follows it.
CREATE TABLE deploy_environments (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE deploy_apps (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE deploy_registrations (
    app TEXT NOT NULL,
    environment TEXT NOT NULL,
    spec TEXT NOT NULL,
    PRIMARY KEY (app, environment)
);
CREATE TABLE deploy_releases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app TEXT NOT NULL,
    environment TEXT NOT NULL,
    state TEXT NOT NULL,
    spec TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX deploy_releases_registration ON deploy_releases (app, environment, id);
CREATE INDEX deploy_releases_state ON deploy_releases (state);
CREATE TABLE deploy_work (
    release INTEGER PRIMARY KEY,
    due_at INTEGER NOT NULL,
    lease_owner TEXT NOT NULL DEFAULT '',
    lease_until INTEGER NOT NULL DEFAULT 0
);
