-- dbq agents (2026-11-06): the goal driver's hot reads, each tick (1.5 s), sought on an index.
-- goal_items_by_status serves the reads of an agent's goals by status: goals::adopt's goal under way
-- (each agent with a goal set, each tick: 32 -> 4.5 us; 61 -> 4.8 us for a 338-goal stack),
-- harness::lineage::on_goals (129 -> 12 us), the goals still to go of settle, run_from_here and
-- restore (26 -> 5.5 us), and their status updates; with pos, in the order the old index gave them.
-- CREATE INDEX at production's size: 4 ms with the table cached.
CREATE INDEX IF NOT EXISTS goal_items_by_status ON goal_items (agent, status, pos);
