-- What a goal stack hands to other agents, found from the agent it went to (G9 step 3, goal a2a44298; T-T1.5.1):
-- each change to a stack asks whether any goal is handed to that agent, as a subagent's (`subagent`) or by a
-- delegation (`delegated_to`), as it takes its turn (`goals::spans`); and a subagent gone brings back what it held
-- by the same column (`goals::subagents::release`). Without these, each look read every stack's goals.
CREATE INDEX IF NOT EXISTS goal_items_by_subagent ON goal_items (subagent) WHERE subagent IS NOT NULL;
CREATE INDEX IF NOT EXISTS goal_items_by_delegated_to ON goal_items (delegated_to) WHERE delegated_to IS NOT NULL;
