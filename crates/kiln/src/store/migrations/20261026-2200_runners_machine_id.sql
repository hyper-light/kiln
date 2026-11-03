-- The machine each runner's name belongs to (docs/architecture.md, "Home network"): a hash of the id
-- its OS keeps for it, as its runner last said (RunnerFacts.machine_id, docs/ci.md W13-R01). A
-- machine joining under a name another machine's runner has gets a name of its own. Nullable, and
-- older builds never name it.
ALTER TABLE agents ADD COLUMN machine_id TEXT;
