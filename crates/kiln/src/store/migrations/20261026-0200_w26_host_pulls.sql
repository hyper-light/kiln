-- The hosts' pull requests, mirrored (docs/ci.md W26-R01, R20): each GitHub pull request, GitLab
-- merge request and Bitbucket pull request of a repository kiln has on that host, as last read
-- (webhooks, and polling with ETags: kiln_ci::mirror). Each head it has had (its revisions, in the
-- order kiln saw them), the commits its latest head brings besides the head (oldest first), and
-- where it merged join Ship's read model as a change's commits, beside kiln's own diffs, so a
-- pull request is a Ship item by commit from its first push. Principals are `<host>:<login>`.
CREATE TABLE host_pulls (
    repo_id TEXT NOT NULL,
    host TEXT NOT NULL,
    number INTEGER NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    author TEXT NOT NULL DEFAULT '',
    -- open, draft, merged or closed
    state TEXT NOT NULL DEFAULT 'open',
    url TEXT NOT NULL DEFAULT '',
    base TEXT NOT NULL DEFAULT '',
    -- JSON: the reviewers asked and not yet heard from ([principal])
    requested TEXT NOT NULL DEFAULT '[]',
    -- JSON: each review ([{by, verdict, at}]: verdict approve, request_changes or comment; at, unix s)
    reviews TEXT NOT NULL DEFAULT '[]',
    -- Open threads on the current diff, and the last comment's author and words
    threads INTEGER NOT NULL DEFAULT 0,
    said_by TEXT NOT NULL DEFAULT '',
    said TEXT NOT NULL DEFAULT '',
    merged_sha TEXT NOT NULL DEFAULT '',
    host_updated TEXT NOT NULL DEFAULT '',
    first_seen INTEGER NOT NULL,
    PRIMARY KEY (repo_id, host, number)
);

CREATE TABLE host_pull_heads (
    repo_id TEXT NOT NULL,
    host TEXT NOT NULL,
    number INTEGER NOT NULL,
    n INTEGER NOT NULL,
    sha TEXT NOT NULL,
    seen_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, host, number, n)
);
CREATE INDEX host_pull_heads_sha ON host_pull_heads (sha);

CREATE TABLE host_pull_commits (
    repo_id TEXT NOT NULL,
    host TEXT NOT NULL,
    number INTEGER NOT NULL,
    sha TEXT NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (repo_id, host, number, sha)
);
CREATE INDEX host_pull_commits_sha ON host_pull_commits (sha);
CREATE INDEX host_pulls_merged ON host_pulls (merged_sha);

DROP VIEW ship_commit_changes;
CREATE VIEW ship_commit_changes AS
    SELECT d.repo_id AS repo_id, v.commit_sha AS sha, d.id AS diff_id, v.n AS n, 1 AS head, 'kiln' AS host, d.created_at AS created_at, d.number AS number, d.kind AS kind
      FROM diff_versions v JOIN diffs d ON d.id = v.diff_id
    UNION ALL
    SELECT d.repo_id, c.sha, d.id, c.n, 0, 'kiln', d.created_at, d.number, d.kind FROM diff_version_commits c JOIN diffs d ON d.id = c.diff_id
    UNION ALL
    SELECT d.repo_id, d.landed_commit, d.id, (SELECT MAX(v.n) FROM diff_versions v WHERE v.diff_id = d.id), 1, 'kiln', d.created_at, d.number, d.kind
      FROM diffs d WHERE d.landed_commit IS NOT NULL AND d.landed_commit <> ''
    UNION ALL
    SELECT h.repo_id, h.sha, 'pull:' || h.host || ':' || h.number, h.n, 1, h.host, p.first_seen, h.number, 'change_request'
      FROM host_pull_heads h JOIN host_pulls p ON p.repo_id = h.repo_id AND p.host = h.host AND p.number = h.number
    UNION ALL
    SELECT c.repo_id, c.sha, 'pull:' || c.host || ':' || c.number, (SELECT MAX(h.n) FROM host_pull_heads h WHERE h.repo_id = c.repo_id AND h.host = c.host AND h.number = c.number), 0, c.host, p.first_seen, c.number, 'change_request'
      FROM host_pull_commits c JOIN host_pulls p ON p.repo_id = c.repo_id AND p.host = c.host AND p.number = c.number
    UNION ALL
    SELECT p.repo_id, p.merged_sha, 'pull:' || p.host || ':' || p.number, (SELECT MAX(h.n) FROM host_pull_heads h WHERE h.repo_id = p.repo_id AND h.host = p.host AND h.number = p.number), 1, p.host, p.first_seen, p.number, 'change_request'
      FROM host_pulls p WHERE p.merged_sha <> '';

CREATE TRIGGER ship_review_pulls_ai AFTER INSERT ON host_pulls BEGIN
    INSERT OR IGNORE INTO ship_order (repo_id, item) SELECT NEW.repo_id, NEW.merged_sha WHERE NEW.merged_sha <> '';
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = 'pull:' || NEW.host || ':' || NEW.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_pulls_au AFTER UPDATE ON host_pulls BEGIN
    INSERT OR IGNORE INTO ship_order (repo_id, item) SELECT NEW.repo_id, NEW.merged_sha WHERE NEW.merged_sha <> '';
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT OLD.repo_id, OLD.merged_sha, (SELECT n FROM ship_seq) WHERE OLD.merged_sha <> '' ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = 'pull:' || NEW.host || ':' || NEW.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_pulls_ad AFTER DELETE ON host_pulls BEGIN
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT h.repo_id, h.sha, (SELECT n FROM ship_seq) FROM host_pull_heads h WHERE h.repo_id = OLD.repo_id AND h.host = OLD.host AND h.number = OLD.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT c.repo_id, c.sha, (SELECT n FROM ship_seq) FROM host_pull_commits c WHERE c.repo_id = OLD.repo_id AND c.host = OLD.host AND c.number = OLD.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT OLD.repo_id, OLD.merged_sha, (SELECT n FROM ship_seq) WHERE OLD.merged_sha <> '' ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_heads_ai AFTER INSERT ON host_pull_heads BEGIN
    INSERT OR IGNORE INTO ship_order (repo_id, item) VALUES (NEW.repo_id, NEW.sha);
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = 'pull:' || NEW.host || ':' || NEW.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_heads_ad AFTER DELETE ON host_pull_heads BEGIN
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) VALUES (OLD.repo_id, OLD.sha, (SELECT n FROM ship_seq)) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = 'pull:' || OLD.host || ':' || OLD.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_pull_commits_ai AFTER INSERT ON host_pull_commits BEGIN
    INSERT OR IGNORE INTO ship_order (repo_id, item) VALUES (NEW.repo_id, NEW.sha);
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = 'pull:' || NEW.host || ':' || NEW.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
CREATE TRIGGER ship_review_pull_commits_ad AFTER DELETE ON host_pull_commits BEGIN
    UPDATE ship_seq SET n = n + 1;
    INSERT INTO ship_changes (repo_id, item, seq) VALUES (OLD.repo_id, OLD.sha, (SELECT n FROM ship_seq)) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
    INSERT INTO ship_changes (repo_id, item, seq) SELECT x.repo_id, x.sha, (SELECT n FROM ship_seq) FROM ship_commit_changes x WHERE x.diff_id = 'pull:' || OLD.host || ':' || OLD.number ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq;
END;
