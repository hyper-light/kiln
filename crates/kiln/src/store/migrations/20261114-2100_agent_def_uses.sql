-- Which of a person's own agent definitions stands in for one of kiln's UI agents, for them alone
-- (`crate::agentdefs::uses`, docs/api.md "Agent definitions"; kiln plan 01a1dd2e8618733a87f2793aab1322a8
-- P5.1b): `person` (`owner` or `user:<id>`) has their definition `agent_def_id` started where kiln
-- would start its own `builtin` (`kiln:<name>`) for them. Deleting the definition drops its row. A new
-- table: an older kiln never names it.
CREATE TABLE agent_def_uses (
    person TEXT NOT NULL,
    builtin TEXT NOT NULL,
    agent_def_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (person, builtin)
);
