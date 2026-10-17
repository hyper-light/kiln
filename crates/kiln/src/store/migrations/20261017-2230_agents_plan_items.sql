-- Plan items (docs/plans.md): a plan's work, item by item, each with its description, examples,
-- acceptance criteria and testing. Kept with each version too.
ALTER TABLE plans ADD COLUMN items TEXT NOT NULL DEFAULT '[]';
ALTER TABLE plan_versions ADD COLUMN items TEXT NOT NULL DEFAULT '[]';
