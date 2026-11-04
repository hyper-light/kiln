-- Hosts' pull requests kept fresh as their base moves (docs/ci.md W26-R16). Each replay of a pull
-- request's head (its `n` in host_pull_heads, and that commit) onto its base's tip in kiln's mirror,
-- by `git merge-tree --write-tree`, as a change request's is: the tree git wrote and how it went
-- (clean or conflicted); a clean one's commit, kiln's (its base merged into its head), kept at
-- `refs/kiln/pulls/<host>/<number>/replays/<k>` for Update to push. Nothing moves on the host until
-- someone presses Update (or the repository turns automatic pushes on). Ship's read model notes the
-- pull request's commits when its replays change (Ship's pattern, migration 20261025-0100). A new
-- table only: an older kiln never names it.
CREATE TABLE review_pull_replays (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    host TEXT NOT NULL,
    number INTEGER NOT NULL,
    head_n INTEGER NOT NULL,
    head_sha TEXT NOT NULL,
    onto TEXT NOT NULL,
    onto_name TEXT NOT NULL,
    tree TEXT NOT NULL,
    state TEXT NOT NULL,
    commit_sha TEXT,
    k INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (repo_id, host, number, head_sha, onto)
);
CREATE INDEX review_pull_replays_pull ON review_pull_replays (repo_id, host, number, k);

CREATE TRIGGER ship_review_pull_replays_ai AFTER INSERT ON review_pull_replays BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.repo_id = NEW.repo_id AND x.diff_id = 'pull:' || NEW.host || ':' || NEW.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_pull_replays_au AFTER UPDATE ON review_pull_replays BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.repo_id = NEW.repo_id AND x.diff_id = 'pull:' || NEW.host || ':' || NEW.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
