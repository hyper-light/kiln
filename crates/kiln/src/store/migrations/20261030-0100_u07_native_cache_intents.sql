-- U07: native host-only publication uses the same physical tier, intents and LRU.
-- v1 remains the shipped chunk/assembly plan; unknown typed plans fail closed.
ALTER TABLE runner_cache_intents ADD COLUMN plan_version INTEGER NOT NULL DEFAULT 1;
