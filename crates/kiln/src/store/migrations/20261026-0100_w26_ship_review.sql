-- Review in Ship (docs/ci.md W26-R01, R02): kiln's diffs and change requests join Ship's read model
-- by commit. A change request's version keeps the commits it brings besides its head (oldest
-- first), so they fold under the head's row; a thread says its kind (W26-R11: question, nit, issue,
-- blocking), existing threads being questions. ship_commit_changes says which change each commit is
-- of: each version's head, the other commits of a change request's version, and where a change
-- landed. A commit is a Ship item when it has runs or is a change's (W26-R01); each is numbered in
-- ship_order on first sight, and every record a change's stand is read from notes all its commits
-- in ship_changes (Ship's pattern, migration 20261025-0100). `host` is where the change is reviewed
-- (kiln's own here; the hosts' mirror joins it, W26-R20).
CREATE TABLE diff_version_commits (
    diff_id TEXT NOT NULL,
    n INTEGER NOT NULL,
    sha TEXT NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (diff_id, n, sha)
);
CREATE INDEX diff_version_commits_sha ON diff_version_commits (sha);
CREATE INDEX diffs_landed_commit ON diffs (landed_commit);

ALTER TABLE review_threads ADD COLUMN kind TEXT NOT NULL DEFAULT 'question';

CREATE VIEW ship_commit_changes AS
    SELECT d.repo_id AS repo_id, v.commit_sha AS sha, d.id AS diff_id, v.n AS n, 1 AS head, 'kiln' AS host FROM diff_versions v JOIN diffs d ON d.id = v.diff_id
    UNION ALL
    SELECT d.repo_id, c.sha, d.id, c.n, 0, 'kiln' FROM diff_version_commits c JOIN diffs d ON d.id = c.diff_id
    UNION ALL
    SELECT d.repo_id, d.landed_commit, d.id, (SELECT MAX(v.n) FROM diff_versions v WHERE v.diff_id = d.id), 1, 'kiln' FROM diffs d WHERE d.landed_commit IS NOT NULL AND d.landed_commit <> '';

INSERT OR IGNORE INTO ship_order (repo_id, item) SELECT d.repo_id, v.commit_sha FROM diff_versions v JOIN diffs d ON d.id = v.diff_id ORDER BY v.created_at, d.number, v.n;

CREATE TRIGGER ship_review_versions_ai AFTER INSERT ON diff_versions BEGIN
    INSERT OR IGNORE INTO ship_order (repo_id, item) SELECT d.repo_id, NEW.commit_sha FROM diffs d WHERE d.id = NEW.diff_id;
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_versions_au AFTER UPDATE ON diff_versions BEGIN
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT d.repo_id, OLD.commit_sha, (SELECT n FROM ship_seq) FROM diffs d WHERE d.id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (OLD.diff_id, NEW.diff_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_versions_ad AFTER DELETE ON diff_versions BEGIN
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT d.repo_id, OLD.commit_sha, (SELECT n FROM ship_seq) FROM diffs d WHERE d.id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;

CREATE TRIGGER ship_review_commits_ai AFTER INSERT ON diff_version_commits BEGIN
    INSERT OR IGNORE INTO ship_order (repo_id, item) SELECT d.repo_id, NEW.sha FROM diffs d WHERE d.id = NEW.diff_id;
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_commits_ad AFTER DELETE ON diff_version_commits BEGIN
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT d.repo_id, OLD.sha, (SELECT n FROM ship_seq) FROM diffs d WHERE d.id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;

CREATE TRIGGER ship_review_diffs_au AFTER UPDATE ON diffs BEGIN
    INSERT OR IGNORE INTO ship_order (repo_id, item) SELECT NEW.repo_id, NEW.landed_commit WHERE NEW.landed_commit IS NOT NULL AND NEW.landed_commit <> '';
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT OLD.repo_id, OLD.landed_commit, (SELECT n FROM ship_seq) WHERE OLD.landed_commit IS NOT NULL AND OLD.landed_commit <> '' ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_diffs_ad AFTER DELETE ON diffs BEGIN
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT OLD.repo_id, v.commit_sha, (SELECT n FROM ship_seq) FROM diff_versions v WHERE v.diff_id = OLD.id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT OLD.repo_id, c.sha, (SELECT n FROM ship_seq) FROM diff_version_commits c WHERE c.diff_id = OLD.id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT OLD.repo_id, OLD.landed_commit, (SELECT n FROM ship_seq) WHERE OLD.landed_commit IS NOT NULL AND OLD.landed_commit <> '' ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;

CREATE TRIGGER ship_review_reviewers_ai AFTER INSERT ON diff_reviewers BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_reviewers_au AFTER UPDATE ON diff_reviewers BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (OLD.diff_id, NEW.diff_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_reviewers_ad AFTER DELETE ON diff_reviewers BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;

CREATE TRIGGER ship_review_reviews_ai AFTER INSERT ON reviews BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_reviews_au AFTER UPDATE ON reviews BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (OLD.diff_id, NEW.diff_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_reviews_ad AFTER DELETE ON reviews BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;

CREATE TRIGGER ship_review_threads_ai AFTER INSERT ON review_threads BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_threads_au AFTER UPDATE ON review_threads BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (OLD.diff_id, NEW.diff_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_threads_ad AFTER DELETE ON review_threads BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;

CREATE TRIGGER ship_review_comments_ai AFTER INSERT ON review_comments BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM review_threads t JOIN ship_commit_changes x ON x.diff_id = t.diff_id WHERE t.id = NEW.thread_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_comments_au AFTER UPDATE ON review_comments BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM review_threads t JOIN ship_commit_changes x ON x.diff_id = t.diff_id WHERE t.id IN (OLD.thread_id, NEW.thread_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_comments_ad AFTER DELETE ON review_comments BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM review_threads t JOIN ship_commit_changes x ON x.diff_id = t.diff_id WHERE t.id = OLD.thread_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;

CREATE TRIGGER ship_review_when_ready_ai AFTER INSERT ON land_when_ready BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = NEW.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_when_ready_au AFTER UPDATE ON land_when_ready BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id IN (OLD.diff_id, NEW.diff_id) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_when_ready_ad AFTER DELETE ON land_when_ready BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = OLD.diff_id ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;

-- The land queue names the diffs an entry lands (a JSON list of their ids): their commits' stands
-- read it (Ship's own triggers note the entry's candidate).
CREATE TRIGGER ship_review_land_ai AFTER INSERT ON land_queue BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM json_each(CASE WHEN json_valid(NEW.diffs) THEN NEW.diffs ELSE '[]' END) j JOIN ship_commit_changes x ON x.diff_id = j.value WHERE true ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_land_au AFTER UPDATE ON land_queue BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM json_each(CASE WHEN json_valid(OLD.diffs) THEN OLD.diffs ELSE '[]' END) j JOIN ship_commit_changes x ON x.diff_id = j.value WHERE true ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM json_each(CASE WHEN json_valid(NEW.diffs) THEN NEW.diffs ELSE '[]' END) j JOIN ship_commit_changes x ON x.diff_id = j.value WHERE true ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_review_land_ad AFTER DELETE ON land_queue BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM json_each(CASE WHEN json_valid(OLD.diffs) THEN OLD.diffs ELSE '[]' END) j JOIN ship_commit_changes x ON x.diff_id = j.value WHERE true ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
