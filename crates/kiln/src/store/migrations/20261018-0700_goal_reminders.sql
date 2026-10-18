-- Goal stacks remind (docs/goals.md): the goal under way is sent again whenever the agent stops
-- short of it, until it's met. How many times in a row it has been sent without real work since,
-- when it was last sent, and when the agent was first seen at work on it since.
ALTER TABLE goal_items ADD COLUMN reminders INTEGER NOT NULL DEFAULT 0;
ALTER TABLE goal_items ADD COLUMN reminded_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE goal_items ADD COLUMN busy_at INTEGER NOT NULL DEFAULT 0;
-- When you last cleared the stack or skipped its goal: a goal the agent still pursued then is
-- being cleared there, and doesn't join the stack again.
ALTER TABLE goal_stacks ADD COLUMN dropped_at INTEGER NOT NULL DEFAULT 0;
