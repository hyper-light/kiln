-- A plan's steps side by side on a goal stack (docs/goals.md, "A plan's steps side by side";
-- docs/agentic.md P2): the subagent kiln handed a step's goal to (an agent of the stack's agent's
-- own, started for it), kept with the step's goal and its parts while it's at it, and with the goal
-- once it met it. `subagent_back`: 1 once the step came back from its subagent unmet, so the agent
-- takes it itself rather than another subagent.
ALTER TABLE goal_items ADD COLUMN subagent TEXT;
ALTER TABLE goal_items ADD COLUMN subagent_back INTEGER NOT NULL DEFAULT 0;
