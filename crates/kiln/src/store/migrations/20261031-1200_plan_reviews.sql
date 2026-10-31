-- A person's answers to the plans their agents' goal stacks wait on (docs/plans.md, "Reviewing a
-- plan"): `approve` (its steps run), `reject` (they don't: the goal it was made from is skipped) or
-- `revise` (its agent changes it as `words` say, and it comes back to look at again), each for the
-- version they looked at. `key`: the answer asked again under it is the same answer (CP-T3.2).
CREATE TABLE plan_reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    answer TEXT NOT NULL,
    words TEXT,
    goal TEXT,
    by TEXT NOT NULL,
    at INTEGER NOT NULL,
    key TEXT
);
CREATE INDEX plan_reviews_by_plan ON plan_reviews (plan_id, id);
