-- A goal of a program's coordinator's stack under way on another agent, its track's worker
-- (docs/programs.md, "Assignments"): the agent's id, or none. The driver doesn't start it on the
-- coordinator.
ALTER TABLE goal_items ADD COLUMN delegated_to TEXT;
