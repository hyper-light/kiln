-- Research (docs/research.md): a question an agent looks into, its sources and its findings,
-- each finding with the sources it rests on and the counterclaims against it (JSON lists), and
-- why it has no internal or no external sources, when it says so. A plan's planning phase is one
-- (`plan_id`).
CREATE TABLE IF NOT EXISTS research (
    id TEXT PRIMARY KEY,
    question TEXT NOT NULL,
    agent TEXT,
    by TEXT NOT NULL,
    plan_id TEXT,
    sources TEXT NOT NULL DEFAULT '[]',
    findings TEXT NOT NULL DEFAULT '[]',
    skipped TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS research_by_plan ON research (plan_id);
CREATE INDEX IF NOT EXISTS research_by_updated ON research (updated_at);
-- A plan made from a goal is in its planning phase until its agent says it's ready.
ALTER TABLE plans ADD COLUMN phase TEXT NOT NULL DEFAULT 'ready';
-- Goal stacks (docs/goals.md): a goal turned into a plan names it; a part says who made it
-- (`own`: the agent; `checks`: a plan step's criteria and tests; `plan`: a plan's step; `planning`:
-- the planning of a plan; `run`: waiting for a plan run) — a goal whose parts kiln made is met once
-- they all are; and a part that waits for a plan run names the run.
ALTER TABLE goal_items ADD COLUMN planned_as TEXT;
ALTER TABLE goal_items ADD COLUMN part_kind TEXT;
ALTER TABLE goal_items ADD COLUMN waits_on TEXT;
