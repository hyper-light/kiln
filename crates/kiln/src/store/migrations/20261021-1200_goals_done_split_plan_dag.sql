-- Goal stacks (docs/goals.md): a goal split into parts names the goal they're parts of (it comes
-- back after them); a goal made from a plan's step names the step (its plan, the version run, its
-- index from 0, and the run, when one agent per step runs it); a stack paused for a while resumes by
-- itself at `resume_at` (unix ms; 0: only when someone resumes it).
ALTER TABLE goal_items ADD COLUMN part_of TEXT;
ALTER TABLE goal_items ADD COLUMN plan_id TEXT;
ALTER TABLE goal_items ADD COLUMN plan_version INTEGER;
ALTER TABLE goal_items ADD COLUMN plan_step INTEGER;
ALTER TABLE goal_items ADD COLUMN plan_run TEXT;
ALTER TABLE goal_stacks ADD COLUMN resume_at INTEGER NOT NULL DEFAULT 0;
-- The id of the message that last took a goal to its agent: withdrawn from the agent's queue when
-- the goal ends (or goes back among the others) before the agent took it.
ALTER TABLE goal_items ADD COLUMN sent_id TEXT;
-- A plan run of one agent per step (docs/plans.md): the steps each step waits for (a JSON list of
-- their indexes from 0), and the agent a step was given to when it's one that was there already
-- (the agent that ran the plan, say): its goal goes on that agent's stack, and no agent is started.
ALTER TABLE plan_run_steps ADD COLUMN needs TEXT;
ALTER TABLE plan_run_steps ADD COLUMN given_to TEXT;
