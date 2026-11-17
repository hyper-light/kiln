-- What each draw's session of an agent definition's experiment came to (`crate::agentdefs::experiments::measures`,
-- docs/api.md "Experiments"; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1), kept as kiln first reads it: its warm
-- start, where its first start was a warm one (`warm_at`, Unix ms, and `warm_ms`, how long that start took); and once
-- it's over (it ended, or its start was refused), when (`ended_at`, Unix ms), whether it failed (`failed`, 1 or 0) and
-- its microVM's peak memory (`peak_bytes`). Each is written to kiln's observability as it's kept, once. New columns: an
-- older kiln never names them. A draw whose start was refused before them is over, failed, at its draw.
ALTER TABLE agent_def_experiment_draws ADD COLUMN warm_at INTEGER;
ALTER TABLE agent_def_experiment_draws ADD COLUMN warm_ms REAL;
ALTER TABLE agent_def_experiment_draws ADD COLUMN ended_at INTEGER;
ALTER TABLE agent_def_experiment_draws ADD COLUMN failed INTEGER;
ALTER TABLE agent_def_experiment_draws ADD COLUMN peak_bytes INTEGER;
UPDATE agent_def_experiment_draws SET ended_at = at, failed = 1 WHERE refused IS NOT NULL;
