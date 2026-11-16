-- Sessions started from agent definitions (`crate::agentdefs::sessions`, docs/api.md "Agent definitions"; kiln plan
-- 01a1dd2e… P5.2's page): session `session_id` started from definition `agent_def_id` (one of a person's, or kiln's
-- own, `kiln:<name>`) at `version` (kiln's own: 0), acting for `person` (`owner` or `user:<id>`; empty for none),
-- placed on `target` (`local`, `agent:<machine>`), at `at` (Unix ms). Written as kiln opens it, never from what was
-- asked; deleting the definition drops its rows. A new table: an older kiln never names it.
CREATE TABLE agent_def_sessions (
    session_id TEXT PRIMARY KEY,
    agent_def_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    person TEXT NOT NULL,
    target TEXT NOT NULL,
    at INTEGER NOT NULL
);

CREATE INDEX agent_def_sessions_by_def ON agent_def_sessions (agent_def_id, at);
