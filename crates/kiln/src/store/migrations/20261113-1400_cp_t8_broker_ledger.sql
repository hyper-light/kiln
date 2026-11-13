-- The run-lease broker's ledger (CP-T8.3, Shards plan step 33's step 5;
-- crates/kiln/src/secrets/broker.rs): each run's key, when it ended and the key a move replaced
-- it with; each lease held for a run until its revocation went through; and when a value ends
-- where its provider said. Kept so that a run's leases are revoked at its end, and nothing is
-- leased under its key after, whatever restarted between.
CREATE TABLE IF NOT EXISTS broker_runs (
    key TEXT PRIMARY KEY,
    -- When it ended (Unix ms); NULL while it runs.
    ended_ms INTEGER,
    -- The run's key since a move replaced this one.
    replaced_by TEXT
);

CREATE TABLE IF NOT EXISTS broker_leases (
    lease_id TEXT PRIMARY KEY,
    key TEXT NOT NULL,
    reference TEXT NOT NULL,
    -- When its revocation went through (Unix ms); NULL until then.
    revoked_ms INTEGER
);

CREATE INDEX IF NOT EXISTS broker_leases_key ON broker_leases (key, revoked_ms);

CREATE TABLE IF NOT EXISTS broker_ends (
    reference TEXT PRIMARY KEY,
    end_ms INTEGER NOT NULL
);
