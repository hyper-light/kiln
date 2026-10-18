-- Goal stacks remind (docs/goals.md): the goal under way is sent again whenever the agent stops
-- short of it, until it's met. How many times in a row it has been sent without real work since,
-- when it was last sent, and when the agent was first seen at work on it since.
ALTER TABLE goal_items ADD COLUMN reminders INTEGER NOT NULL DEFAULT 0;
ALTER TABLE goal_items ADD COLUMN reminded_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE goal_items ADD COLUMN busy_at INTEGER NOT NULL DEFAULT 0;
-- When you last cleared the stack or skipped its goal: a goal the agent still pursued then is
-- being cleared there, and doesn't join the stack again.
ALTER TABLE goal_stacks ADD COLUMN dropped_at INTEGER NOT NULL DEFAULT 0;

-- Stacks the old rules paused by themselves (not you) run again under the new ones, which keep
-- the agent on its goal: a goal that wasn't met is under way again, and the goal under way is sent
-- again (it has no reminder yet).
UPDATE goal_items SET status = 'active', reason = NULL, ended_at = NULL
 WHERE status = 'unmet'
   AND agent IN (SELECT agent FROM goal_stacks WHERE state = 'paused' AND reason LIKE 'A goal wasn''t met:%')
   AND ended_at = (SELECT MAX(j.ended_at) FROM goal_items j WHERE j.agent = goal_items.agent AND j.status = 'unmet')
   AND NOT EXISTS (SELECT 1 FROM goal_items k WHERE k.agent = goal_items.agent AND k.status = 'active');
UPDATE goal_stacks SET state = 'running', reason = NULL
 WHERE state = 'paused'
   AND (reason LIKE 'A goal wasn''t met:%' OR reason IN (
        'The agent didn''t take the goal up.',
        'The agent stopped. Restart it, then start the stack again.',
        'The agent''s machine isn''t connected.',
        'The goal was paused in the agent: resume it there, or start the stack again.'))
   AND EXISTS (SELECT 1 FROM goal_items i WHERE i.agent = goal_stacks.agent AND i.status IN ('pending', 'active'));
