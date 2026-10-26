-- Goal stacks as a graph (docs/goals.md, "What a goal waits for"; docs/agentic.md G1): what each
-- goal waits for, a JSON list of the ids of goals of its stack (NULL: it says nothing yet, so the
-- agent orders it while subagents can take goals, else it waits for the goal still to go before
-- it), and who said so (`you`, `the agent`, another agent's name, or `kiln` when it couldn't get it
-- ordered and it goes in turn). A stack runs its goals side by side unless a person turned that off
-- (NULL: on).
ALTER TABLE goal_items ADD COLUMN needs TEXT;
ALTER TABLE goal_items ADD COLUMN needs_by TEXT;
ALTER TABLE goal_stacks ADD COLUMN side_by_side INTEGER;
