-- W05-R25, R34 (docs/ci.md, docs/needs.md §8, E3.7): the epoch the CI engine last took. Each start
-- takes the next: the cluster group's term (which each start's campaign raises) or one past the
-- last, whichever is greater, so it rises on every start and with every new leader, the group on
-- or off. Grants keep the epoch they were made in.
CREATE TABLE IF NOT EXISTS ci_epoch (one INTEGER PRIMARY KEY CHECK (one = 1), epoch INTEGER NOT NULL);
INSERT OR IGNORE INTO ci_epoch (one, epoch) VALUES (1, 0);
