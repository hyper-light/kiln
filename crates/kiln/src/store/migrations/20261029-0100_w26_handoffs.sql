-- Ship's Hand off (docs/ci.md W26-R18, R02): what of a commit was handed to an agent, to whom, by
-- whom and when. `what` is `checks` (its failure, to its author's agent or to what looks after the
-- repository: `incident` names the attachment's), `threads` or `conflicts` (a change's, at
-- revision `version`, to its author's agent). A person's failure handed off reads "<to_name>
-- fixing" and needs no one; a change handed off reads fixing until a new revision replaces it.
-- Each hand-off notes its commit in ship_changes (Ship's pattern, migration 20261025-0100).
CREATE TABLE ship_handoffs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id TEXT NOT NULL,
    sha TEXT NOT NULL,
    what TEXT NOT NULL,
    to_name TEXT NOT NULL,
    agent TEXT,
    incident TEXT,
    version INTEGER,
    by TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX ship_handoffs_commit ON ship_handoffs (repo_id, sha);
CREATE TRIGGER ship_handoffs_ai AFTER INSERT ON ship_handoffs BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) VALUES (NEW.repo_id, NEW.sha, (SELECT n FROM ship_seq)) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_handoffs_au AFTER UPDATE ON ship_handoffs BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) VALUES (OLD.repo_id, OLD.sha, (SELECT n FROM ship_seq)) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
CREATE TRIGGER ship_handoffs_ad AFTER DELETE ON ship_handoffs BEGIN UPDATE ship_seq SET n = n + 1; INSERT INTO ship_changes (repo_id, item, seq) VALUES (OLD.repo_id, OLD.sha, (SELECT n FROM ship_seq)) ON CONFLICT (repo_id, item) DO UPDATE SET seq = excluded.seq; END;
