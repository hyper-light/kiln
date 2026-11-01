-- A repository's switch that lets agents fix code from agents' feedback (docs/ci.md W26-R18): set
-- on its page in Repos, off until turned on (a repository with no row is off), kept with who set
-- it and when. While it's off, an agent's comment on an agent's change starts nothing; on, it goes
-- to that change's agent (kiln_ci::review::agents). A new table: an older kiln never names it.
CREATE TABLE review_agent_fixes (
    repo_id TEXT PRIMARY KEY,
    agents_fix INTEGER NOT NULL,
    set_by TEXT NOT NULL,
    set_at INTEGER NOT NULL
);
