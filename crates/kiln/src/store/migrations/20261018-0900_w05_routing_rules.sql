-- W05-R21 (docs/ci.md): routing rules set through the API, for the server and for organizations.
-- The server's configuration (`[[ci.routing]]`) and repositories' `.kiln/routing.yaml` hold the
-- rest.
CREATE TABLE ci_routing_rules (
  scope TEXT PRIMARY KEY,               -- server, org:<owner>
  rules TEXT NOT NULL,                  -- JSON array of rules
  updated_at INTEGER NOT NULL,
  updated_by TEXT NOT NULL
);
