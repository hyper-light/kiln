-- The name people see for a runner (docs/machines.md): a runner that enrolls with its machine id
-- is known by that id everywhere (its `name`, the key), and shown by this. None for a runner
-- known by its name, which is shown by it. Nullable, and older builds never name it.
ALTER TABLE agents ADD COLUMN label TEXT;
