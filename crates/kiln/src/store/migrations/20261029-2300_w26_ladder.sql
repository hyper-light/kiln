-- The conflict ladder (docs/ci.md W26-R17). Each rung's try at each conflict of a conflicted replay
-- (review_replays, review_conflicts): its rung (structural, remembered, agent), who made it, how it
-- went (working, kept with why, or resolved: its lines, an agent's explanation, the revision it was
-- committed as and kept at refs/kiln/resolve/…, the workflows its affected tests ran and their
-- outcome: pending, passed, failed, none_to_run). Per replay, the ladder's state: the resolutions
-- tested together (the revision, its workflows, their outcome), who accepted it and when (with the
-- statement W27-R04 signs it with, null until it does), and the version it became. Resolutions a
-- person accepted, by their hunk's key (kiln_ci::ladder::key), per repository: the next replay
-- whose hunk has the key is offered them. And each repository's two switches, off until turned on
-- on its page in Repos: sending an agent to a runner that has the repository, and letting a
-- resolution whose tests passed go on without a person. Ship's read model notes the commits of a
-- change whose ladder moves (Ship's pattern, migration 20261025-0100). New tables only: an older
-- kiln never names them.
CREATE TABLE review_resolutions (
    id TEXT PRIMARY KEY,
    replay_id TEXT NOT NULL,
    conflict_id TEXT NOT NULL,
    diff_id TEXT NOT NULL,
    rung TEXT NOT NULL,
    made_by TEXT NOT NULL,
    state TEXT NOT NULL,
    why TEXT,
    lines TEXT,
    explanation TEXT,
    commit_sha TEXT,
    started TEXT,
    tests TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX review_resolutions_replay ON review_resolutions (replay_id, conflict_id, created_at);
CREATE INDEX review_resolutions_tests ON review_resolutions (tests);

CREATE TABLE review_ladders (
    replay_id TEXT PRIMARY KEY,
    diff_id TEXT NOT NULL,
    state TEXT NOT NULL,
    why TEXT,
    whole_sha TEXT,
    whole_started TEXT,
    whole_tests TEXT,
    accepted_by TEXT,
    accepted_at INTEGER,
    accepted_statement TEXT,
    version INTEGER,
    updated_at INTEGER NOT NULL
);
CREATE INDEX review_ladders_diff ON review_ladders (diff_id);

CREATE TABLE review_remembered (
    repo_id TEXT NOT NULL,
    hunk_key TEXT NOT NULL,
    lines TEXT NOT NULL,
    resolution_id TEXT,
    accepted_by TEXT NOT NULL,
    accepted_at INTEGER NOT NULL,
    used INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (repo_id, hunk_key)
);

CREATE TABLE review_ladder_switches (
    repo_id TEXT PRIMARY KEY,
    send_agent INTEGER NOT NULL DEFAULT 0,
    automatic INTEGER NOT NULL DEFAULT 0,
    set_by TEXT NOT NULL,
    set_at INTEGER NOT NULL
);

CREATE TRIGGER ship_review_resolutions_ai AFTER INSERT ON review_resolutions BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_resolutions_au AFTER UPDATE ON review_resolutions BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (OLD.diff_id, NEW.diff_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_ladders_ai AFTER INSERT ON review_ladders BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_ladders_au AFTER UPDATE ON review_ladders BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (OLD.diff_id, NEW.diff_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
