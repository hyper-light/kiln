-- Whether a goal stack plans its big goals first (docs/goals.md, "Big goals"): a goal of the
-- agent's own longer than a couple of sentences is researched and planned before it's worked on,
-- as Make a plan does, its planning going to the agent in its place. NULL: as by default, on; 0:
-- off, each goal goes to the agent as it is.
ALTER TABLE goal_stacks ADD COLUMN plan_big INTEGER;
