-- CP-T3.2 (the twelfth adversarial check's): what tells a goal stack's change asked again (its
-- answer lost) from a new one.
-- The turn of its agent's an item was ended in (its harness's `turn_started`, on the clock of the
-- agent's machine): an end asked again is matched to the turn it came from on that clock alone,
-- never against the server's.
ALTER TABLE goal_items ADD COLUMN ended_turn INTEGER;
-- The ids of the stack's newest controls and ends asked for under a key (a JSON array): asked
-- again under the same key, one is answered with the stack as it is, whatever changed since.
ALTER TABLE goal_stacks ADD COLUMN asked TEXT;
