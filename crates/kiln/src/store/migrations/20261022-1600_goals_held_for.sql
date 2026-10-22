-- A goal stack its agent paused for a person, until someone resumes it (docs/goals.md): what waits
-- on you wherever you are in kiln (the agent's `held`). '' for any such pause; a plan's id when it
-- waits for you to look at that plan first (`plans_ready` with `then: review`). Any other change of
-- the stack's state lets it go.
ALTER TABLE goal_stacks ADD COLUMN held_for TEXT;
