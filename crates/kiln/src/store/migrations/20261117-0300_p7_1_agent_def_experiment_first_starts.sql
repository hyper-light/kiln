-- Each draw's session's first start of any kind (`crate::agentdefs::experiments::measures`, docs/api.md "Experiments";
-- kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.1's "start cold/template/warm"), kept as kiln first reads it, as its
-- warm start is: when it began (`first_at`, Unix ms), how long it took (`first_ms`) and its kind (`first_kind`: cold,
-- template, warm, swapped or other). Written to kiln's observability as it's kept, once. New columns: an older kiln
-- never names them.
ALTER TABLE agent_def_experiment_draws ADD COLUMN first_at INTEGER;
ALTER TABLE agent_def_experiment_draws ADD COLUMN first_ms REAL;
ALTER TABLE agent_def_experiment_draws ADD COLUMN first_kind TEXT;
