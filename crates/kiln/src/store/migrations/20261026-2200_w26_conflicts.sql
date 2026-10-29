-- Conflicts in the diff (docs/ci.md W26-R12). Each replay of a change's latest revision onto where
-- it lands (its trunk's tip, or the open diff below it), with `git merge-tree --write-tree`, kept by
-- revision and by what it went onto: the tree git wrote and how it went (clean, conflicted, or
-- resolved once Commit resolution made a revision of it: which, by whom, when). Each conflict a
-- replay left, where it falls: its file and kind (text, binary, modify_delete, delete_modify,
-- other); a text conflict's place among its file's and its lines in this change's file; main's
-- side, this side and the base (JSON); git's own words; a fingerprint (the same conflict in a later
-- replay of the revision keeps what was resolved); an agent's resolution where one is recorded; and
-- the resolution taken (main, this, both, agent, edit, with the lines a text conflict resolves to),
-- by whom and when. Replays as main moves and the conflict ladder (W26-R16, R17) keep theirs here
-- too. Ship's read model notes every commit of a change whose replays or conflicts change (Ship's
-- pattern, migration 20261025-0100). New tables only: an older kiln never names them.
CREATE TABLE review_replays (
    id TEXT PRIMARY KEY,
    diff_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    commit_sha TEXT NOT NULL,
    onto TEXT NOT NULL,
    onto_name TEXT NOT NULL,
    tree TEXT NOT NULL,
    state TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    resolved_version INTEGER,
    resolved_by TEXT,
    resolved_at INTEGER,
    UNIQUE (diff_id, version, onto)
);
CREATE INDEX review_replays_diff ON review_replays (diff_id, created_at);

CREATE TABLE review_conflicts (
    id TEXT PRIMARY KEY,
    replay_id TEXT NOT NULL,
    diff_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    file TEXT NOT NULL,
    kind TEXT NOT NULL,
    hunk INTEGER,
    line INTEGER,
    end_line INTEGER,
    main TEXT,
    this TEXT,
    base TEXT,
    said TEXT,
    fingerprint TEXT NOT NULL,
    agent_take TEXT,
    agent_lines TEXT,
    agent_by TEXT,
    agent_at INTEGER,
    take TEXT,
    lines TEXT,
    resolved_by TEXT,
    resolved_at INTEGER
);
CREATE INDEX review_conflicts_replay ON review_conflicts (replay_id, position);
CREATE INDEX review_conflicts_fingerprint ON review_conflicts (fingerprint);

CREATE TRIGGER ship_review_replays_ai AFTER INSERT ON review_replays BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_replays_au AFTER UPDATE ON review_replays BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (OLD.diff_id, NEW.diff_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_replays_ad AFTER DELETE ON review_replays BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;

CREATE TRIGGER ship_review_conflicts_ai AFTER INSERT ON review_conflicts BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_conflicts_au AFTER UPDATE ON review_conflicts BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (OLD.diff_id, NEW.diff_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_conflicts_ad AFTER DELETE ON review_conflicts BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
