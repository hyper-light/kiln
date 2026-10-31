-- U10: one protected action and reservation ledger for jobs and releases. Policy stays in
-- environment.policy; admission is serialized by the host product database, never by a map.
CREATE TABLE environment_flows (
 id TEXT PRIMARY KEY,
 environment_key BLOB NOT NULL,
 repo_id TEXT NOT NULL,
 run_id TEXT NOT NULL,
 run_attempt INTEGER NOT NULL,
 local_environment TEXT NOT NULL COLLATE NOCASE,
 app TEXT NOT NULL,
 version TEXT NOT NULL,
 sha TEXT NOT NULL,
 actor TEXT NOT NULL,
 created_by TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 release_id INTEGER,
 release_fingerprint TEXT,
 UNIQUE(repo_id, run_id, run_attempt, local_environment)
);
CREATE TABLE environment_action_reviews (
 action_id TEXT NOT NULL,
 principal TEXT NOT NULL,
 approved INTEGER NOT NULL,
 comment TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 PRIMARY KEY(action_id, principal)
);
CREATE TABLE environment_reservations (
 environment_key BLOB NOT NULL,
 action_id TEXT NOT NULL,
 holder TEXT NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN ('job','release')),
 object_id TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 PRIMARY KEY(environment_key, holder)
);
CREATE INDEX environment_reservations_action ON environment_reservations(environment_key, action_id);
-- An explicit saved server binding delegates releases from this repository environment.
-- A versioned config reference alone does not grant deployment authority on another scope.
CREATE TABLE environment_policy_bindings (
 repo_id TEXT NOT NULL,
 local_environment TEXT NOT NULL COLLATE NOCASE,
 environment_key BLOB NOT NULL,
 created_by TEXT NOT NULL,
 PRIMARY KEY(repo_id,local_environment)
);
