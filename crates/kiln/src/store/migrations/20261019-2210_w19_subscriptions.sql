-- W19-R06: a person's own notification rules (repository events, to their inbox or their email).
CREATE TABLE IF NOT EXISTS notify_subscriptions (
    user_id TEXT PRIMARY KEY,
    rules TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
