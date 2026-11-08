-- What each reach run used where it ran (docs/reach.md, "Sizes from history"; docs/needs.md E6;
-- audit H3). `run_sizes`: one row a run, under its history key (`key`, the hash of its repository
-- and command as keyed), on `platform` (`linux/x86_64`) and `runner`, both the server's own record
-- of the runner that measured it; what it asked (`ask`), the size history gave it (`size`, null:
-- placed by its ask) and what it used (`used`), each as kiln-ci's JSON; `at` when it was kept (ms
-- since the epoch). Rows go with the runs' prune after `[retention] days`.
-- `run_size_rollups`: what a key's runs on a platform say (`kiln_ci::sizes::Rollup`, JSON), folded
-- in as each run is kept in the same transaction, so a prediction reads one row and never the runs;
-- the prune folds again only the keys it touched. New tables: older builds never name them.
CREATE TABLE run_sizes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    key TEXT NOT NULL,
    repo TEXT NOT NULL,
    command TEXT NOT NULL,
    platform TEXT NOT NULL,
    runner TEXT NOT NULL,
    ask TEXT NOT NULL DEFAULT '{}',
    size TEXT,
    used TEXT NOT NULL DEFAULT '{}',
    at INTEGER NOT NULL
);
CREATE INDEX run_sizes_by_key ON run_sizes (key, platform);
CREATE INDEX run_sizes_by_at ON run_sizes (at);
CREATE TABLE run_size_rollups (
    key TEXT NOT NULL,
    platform TEXT NOT NULL,
    rollup TEXT NOT NULL,
    PRIMARY KEY (key, platform)
);
