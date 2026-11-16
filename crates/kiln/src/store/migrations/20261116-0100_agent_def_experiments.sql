-- A person's experiments on their own agent definitions (`crate::agentdefs::experiments`, docs/api.md "Agent
-- definitions"; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1): splits of a definition's new sessions between
-- deployments, several to a definition, one of them live at a time. Each is its definition's owner's (`owner`, as
-- `agent_defs.owner`), named `name`, `live`, `paused`, `promoted` or `stopped` since `state_at`, started at
-- `started_at` (both Unix ms) by `by`. Its split's version (`split_version`, from 1) goes up with each change of its
-- weights. Its draws come from `seed`, a random number kiln made as it started, and each draw's index (`draws` is how
-- many it has made: `kiln_ci::split::draw_parts`). `promoted` names the variant promoted, once one was. New tables:
-- an older kiln never names them.
CREATE TABLE agent_def_experiments (
    id TEXT PRIMARY KEY,
    agent_def_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    state TEXT NOT NULL,
    state_at INTEGER NOT NULL,
    split_version INTEGER NOT NULL DEFAULT 1,
    seed INTEGER NOT NULL,
    draws INTEGER NOT NULL DEFAULT 0,
    promoted TEXT,
    started_at INTEGER NOT NULL,
    by TEXT NOT NULL
);

CREATE INDEX agent_def_experiments_by_def ON agent_def_experiments (agent_def_id, started_at);
CREATE INDEX agent_def_experiments_by_owner ON agent_def_experiments (owner, started_at);
-- One live at a time on a definition: a new session of it is drawn by that one's weights.
CREATE UNIQUE INDEX agent_def_experiments_one_live ON agent_def_experiments (agent_def_id) WHERE state = 'live';

-- Its variants, in its order (`position`), each a deployment its new sessions start in: `engine` `microvm` or
-- `host`; a microVM's `agentfile` (`agentfile:<id>` or `harness-<harness>`) and size (`cpus`, `memory` in bytes); its
-- `model` (none: the definition's). `weight` is a whole percent of the split (its variants' add up to 100); `name`,
-- none: what sets it apart from the others, as the UI says it.
CREATE TABLE agent_def_experiment_variants (
    experiment_id TEXT NOT NULL,
    id TEXT NOT NULL,
    position INTEGER NOT NULL,
    name TEXT,
    weight INTEGER NOT NULL,
    engine TEXT NOT NULL,
    agentfile TEXT,
    cpus INTEGER,
    memory INTEGER,
    model TEXT,
    PRIMARY KEY (experiment_id, id)
);

-- Its criteria, in its order: each a query in kiln's monitoring syntax (`query`, as kiln's monitors take one, its
-- comparison apart) compared by `op` (`>`, `>=`, `==`, `!=`, `<`, `<=`) with `bound`, as its person wrote them.
CREATE TABLE agent_def_experiment_criteria (
    experiment_id TEXT NOT NULL,
    id TEXT NOT NULL,
    position INTEGER NOT NULL,
    name TEXT NOT NULL,
    query TEXT NOT NULL,
    op TEXT NOT NULL,
    bound REAL NOT NULL,
    PRIMARY KEY (experiment_id, id)
);

-- Its draws, each its own (`draw`, from 0): the variant a new session of its definition got as it started, the
-- split's version then, when (`at`, Unix ms), and what came of it: its session (`session_id`), or why its start was
-- refused (`refused`), that variant's failure.
CREATE TABLE agent_def_experiment_draws (
    experiment_id TEXT NOT NULL,
    draw INTEGER NOT NULL,
    variant_id TEXT NOT NULL,
    split_version INTEGER NOT NULL,
    at INTEGER NOT NULL,
    session_id TEXT,
    refused TEXT,
    PRIMARY KEY (experiment_id, draw)
);

CREATE INDEX agent_def_experiment_draws_by_session ON agent_def_experiment_draws (session_id);
