-- docs/reach.md §7, AUDIT E1/T4 (lb/told): the runs agents are told of, as the server last heard
-- them (`crate::harness::told`), so news and holds outlive a server restart. One row a run: its
-- agent session, its follower's key, the runner whose news of it counts (E8), its latest news
-- (JSON, `RunNews`), when its agent was last handed it (server ms), how many of that runner's news
-- the server had heard then (the window a repeat can come in), whether it was told, and the
-- version it was written at (a later write never loses to an earlier one).
CREATE TABLE IF NOT EXISTS told_runs (
    session TEXT NOT NULL,
    key TEXT NOT NULL,
    runner TEXT NOT NULL,
    news TEXT NOT NULL,
    handed_at INTEGER NOT NULL,
    heard_at INTEGER NOT NULL,
    told INTEGER NOT NULL,
    version INTEGER NOT NULL,
    PRIMARY KEY (session, key)
);
