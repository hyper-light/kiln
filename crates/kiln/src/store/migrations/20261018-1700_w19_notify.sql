-- Notifications (docs/ci.md W19-R06): when each rule last sent to each channel (for `throttle`),
-- and every delivery, how it went.
CREATE TABLE IF NOT EXISTS notify_sent (
    key TEXT PRIMARY KEY,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS notify_deliveries (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    rule TEXT NOT NULL,
    event TEXT NOT NULL,
    run_id TEXT,
    channel TEXT NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    attempts INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS notify_deliveries_run ON notify_deliveries (run_id);
