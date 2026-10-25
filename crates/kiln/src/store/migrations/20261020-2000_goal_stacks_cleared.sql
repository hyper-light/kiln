-- The goals you cleared from a stack last (docs/goals.md, "Clear"), as they were: kept so they can
-- be brought back and run again. A JSON list of the stack's goals; none once brought back.
ALTER TABLE goal_stacks ADD COLUMN cleared TEXT;
