-- CP-T3.2: the id of the goal message the driver is sending an item's agent, kept before it's sent
-- and cleared once the driver knows how it went. After a restart between the two, the driver
-- sends under the same id again, and the agent's inbox takes it once.
ALTER TABLE goal_items ADD COLUMN send_id TEXT;
