-- docs/ci.md W07-R04: a repository's own action policy, narrowing the server's ([ci.actions]).
CREATE TABLE ci_action_policies (
  repo_id TEXT PRIMARY KEY REFERENCES repos(id) ON DELETE CASCADE,
  policy TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  updated_by TEXT NOT NULL
);
