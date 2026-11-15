-- What an agent serving a person's chat turn asked to do that needs that person's click first
-- (`crate::confirms`, docs/api.md "Confirming an agent's act"; kiln plan 01a1dd2e8618733a87f2793aab1322a8
-- P5.6): an op (`op`) with its exact arguments (`input`, canonical JSON), on its target as it was then
-- (`version`), asked by agent session `agent` of `person`. `state`: `asked`, then `confirmed` (the
-- person's click) or `declined`, then `used` (run once, as the person) or `changed` (its target changed
-- before it ran); `dropped` once the person sends that agent their next message. A new table: an older kiln never names it.
CREATE TABLE confirms (
    id TEXT PRIMARY KEY,
    person TEXT NOT NULL,
    agent TEXT NOT NULL,
    op TEXT NOT NULL,
    input TEXT NOT NULL,
    version TEXT NOT NULL,
    says TEXT NOT NULL,
    state TEXT NOT NULL,
    asked_at INTEGER NOT NULL,
    answered_at INTEGER,
    used_at INTEGER
);

CREATE INDEX confirms_person ON confirms (person, state);
CREATE INDEX confirms_agent ON confirms (agent, state);
