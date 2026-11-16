-- Agentfiles built ahead of their sessions (`crate::agentfiles::prebuild`, docs/api.md "Agentfiles"; kiln plan
-- 01a1dd2e6a0b76bbba478c532781feed P4.5): Agentfile `agentfile_id` at `version`, built on runner `runner` beside kiln's
-- harness directory for `harness` (one of kiln's harnesses, as a session of it builds it), and where that build is:
-- `state` `queued`, `building`, `done` (its image, pinned by its digest, `image`) or `failed` (Shards' words, or kiln's,
-- `why`), as of `at` (Unix ms). A save of the Agentfile writes its rows anew; a runner's word changes the row of the version
-- it built alone. Deleting the Agentfile drops its rows. A new table: an older kiln never names it.
CREATE TABLE prebuilds (
    agentfile_id TEXT NOT NULL,
    runner TEXT NOT NULL,
    harness TEXT NOT NULL,
    version INTEGER NOT NULL,
    state TEXT NOT NULL,
    image TEXT,
    why TEXT,
    at INTEGER NOT NULL,
    PRIMARY KEY (agentfile_id, runner, harness)
);
